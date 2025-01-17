import { workerData } from 'node:worker_threads'
import { Readable, pipeline } from 'node:stream'
import { createReadStream, createWriteStream } from 'node:fs'
import { join } from 'node:path'
import { createGunzip, createGzip } from 'node:zlib'
import { promisify } from 'node:util'
import { hostname } from 'node:os'

import { worker } from 'workerpool'
import { T, always, applySpec, assocPath, cond, defaultTo, identity, ifElse, is, pathOr, pipe, propOr } from 'ramda'
import { LRUCache } from 'lru-cache'
import { Rejected, Resolved, fromPromise, of } from 'hyper-async'
import AoLoader from '@permaweb/ao-loader'

import { createLogger } from '../logger.js'

const pipelineP = promisify(pipeline)

function wasmResponse (stream) {
  return new Response(stream, { headers: { 'Content-Type': 'application/wasm' } })
}

/**
 * ###################
 * File Utils
 * ###################
 */

function readWasmFileWith ({ DIR }) {
  return async (moduleId) => {
    const file = join(DIR, `${moduleId}.wasm.gz`)

    return new Promise((resolve, reject) =>
      resolve(pipeline(
        createReadStream(file),
        createGunzip(),
        reject
      ))
    )
  }
}

function writeWasmFileWith ({ DIR, logger }) {
  return async (moduleId, wasmStream) => {
    const file = join(DIR, `${moduleId}.wasm.gz`)

    return pipelineP(
      wasmStream,
      createGzip(),
      createWriteStream(file)
    ).catch((err) => {
      logger('Failed to cache binary for module "%s" in a file. Skipping...', moduleId, err)
    })
  }
}

/**
 * #######################
 * Network Utils
 * #######################
 */

function streamTransactionDataWith ({ fetch, GATEWAY_URL, logger }) {
  return (id) =>
    of(id)
      .chain(fromPromise((id) =>
        fetch(`${GATEWAY_URL}/raw/${id}`)
          .then(async (res) => {
            if (res.ok) return res
            logger(
              'Error Encountered when fetching raw data for transaction \'%s\' from gateway \'%s\'',
              id,
              GATEWAY_URL
            )
            throw new Error(`${res.status}: ${await res.text()}`)
          })
      ))
      .toPromise()
}

/**
 * ##############################
 * #### LRU In-Memory Cache utils
 * ##############################
 */

/**
 * A cache for compiled Wasm Modules
 *
 * @returns {LRUCache<string, WebAssembly.Module>}
 */
function createWasmModuleCache ({ MAX_SIZE }) {
  return new LRUCache({
    /**
     * #######################
     * Capacity Configuration
     * #######################
     */
    max: MAX_SIZE
  })
}

/**
 * A cache for loaded wasm modules,
 * as part of evaluating a stream of messages
 *
 * @returns {LRUCache<string, Function>}
 */
function createWasmInstanceCache ({ MAX_SIZE }) {
  return new LRUCache({
    /**
     * #######################
     * Capacity Configuration
     * #######################
     */
    max: MAX_SIZE
  })
}

export function evaluateWith ({
  wasmInstanceCache,
  wasmModuleCache,
  readWasmFile,
  writeWasmFile,
  streamTransactionData,
  bootstrapWasmInstance,
  logger
}) {
  function maybeCachedModule ({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal }) {
    return of(moduleId)
      .map((moduleId) => wasmModuleCache.get(moduleId))
      .chain((wasm) => wasm
        ? Resolved(wasm)
        : Rejected({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal })
      )
  }

  function maybeStoredBinary ({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal }) {
    logger('Checking for wasm file to load module "%s"...', moduleId)

    return of(moduleId)
      .chain(fromPromise(readWasmFile))
      .chain(fromPromise((stream) =>
        WebAssembly.compileStreaming(wasmResponse(Readable.toWeb(stream)))
      ))
      .bimap(
        () => ({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal }),
        identity
      )
  }

  function loadTransaction ({ moduleId }) {
    logger('Loading wasm transaction "%s"...', moduleId)

    return of(moduleId)
      .chain(fromPromise(streamTransactionData))
      .map((res) => res.body.tee())
      /**
       * Simoultaneously cache the binary in a file
       * and compile to a WebAssembly.Module
       */
      .chain(fromPromise(([s1, s2]) =>
        Promise.all([
          writeWasmFile(moduleId, Readable.fromWeb(s1)),
          WebAssembly.compileStreaming(wasmResponse(s2))
        ])
      ))
      .map(([, res]) => res)
  }

  function loadInstance ({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal }) {
    return maybeCachedModule({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal })
      .bichain(
        /**
         * Potentially Compile the Wasm Module, cache it for next time,
         *
         * then create the Wasm instance
         */
        () => of({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal })
          .chain(maybeStoredBinary)
          .bichain(loadTransaction, Resolved)
          /**
           * Cache the wasm Module in memory for quick access next time
           */
          .map((wasmModule) => {
            logger('Caching compiled WebAssembly.Module for module "%s" in memory, for next time...', moduleId)
            wasmModuleCache.set(moduleId, wasmModule)
            return wasmModule
          }),
        /**
         * Cached instance, so just reuse
         */
        Resolved
      )
      .chain(fromPromise((wasmModule) => bootstrapWasmInstance(wasmModule, gas, memLimit)))
      /**
       * Cache the wasm module for this particular stream,
       * in memory, for quick retrieval next time
       */
      .map((wasmInstance) => {
        wasmInstanceCache.set(streamId, wasmInstance)
        return wasmInstance
      })
  }

  function maybeCachedInstance ({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal }) {
    return of(streamId)
      .map((streamId) => wasmInstanceCache.get(streamId))
      .chain((wasmInstance) => wasmInstance
        ? Resolved(wasmInstance)
        : Rejected({ streamId, moduleId, gas, memLimit, Memory, message, AoGlobal })
      )
  }

  /**
   * Given the previous interaction output,
   * return a function that will merge the next interaction output
   * with the previous.
   */
  const mergeOutput = (prevMemory) => pipe(
    defaultTo({}),
    applySpec({
      /**
       * If the output contains an error, ignore its state,
       * and use the previous evaluation's state
       */
      Memory: ifElse(
        pathOr(undefined, ['Error']),
        always(prevMemory),
        propOr(prevMemory, 'Memory')
      ),
      Error: pathOr(undefined, ['Error']),
      Messages: pathOr([], ['Messages']),
      Spawns: pathOr([], ['Spawns']),
      Output: pipe(
        pathOr('', ['Output']),
        /**
         * Always make sure Output
         * is a string or object
         */
        cond([
          [is(String), identity],
          [is(Object), identity],
          [is(Number), String],
          [T, identity]
        ])
      ),
      GasUsed: pathOr(undefined, ['GasUsed'])
    })
  )

  /**
   * Evaluate a message using the handler that wraps the WebAssembly.Instance,
   * identified by the streamId.
   *
   * If not already instantiated and cached in memory, attempt to use a cached WebAssembly.Module
   * and instantiate the Instance and handler, caching it by streamId
   *
   * If the WebAssembly.Module is not cached, then we check if the binary is cached in a file,
   * then compile it in a WebAssembly.Module, cached in memory, then used to instantiate a
   * new WebAssembly.Instance
   *
   * If not in a file, then the module transaction is downloaded from the Gateway url,
   * cached in a file, compiled, further cached in memory, then used to instantiate a
   * new WebAssembly.Instance and handler
   *
   * Finally, evaluates the message and returns the result of the evaluation.
   */
  return ({ streamId, moduleId, gas, memLimit, name, processId, Memory, message, AoGlobal }) =>
    /**
     * Dynamically load the module, either from cache,
     * or from a file
     */
    maybeCachedInstance({ streamId, moduleId, gas, memLimit, name, processId, Memory, message, AoGlobal })
      .bichain(loadInstance, Resolved)
      /**
       * Perform the evaluation
       */
      .chain((wasmInstance) =>
        of(wasmInstance)
          .map((wasmInstance) => {
            logger('Evaluating message "%s" to process "%s"', name, processId)
            return wasmInstance
          })
          .chain(fromPromise(async (wasmInstance) => wasmInstance(Memory, message, AoGlobal)))
          .bichain(
            /**
             * Map thrown error to a result.error. In this way, the Worker should _never_
             * throw due to evaluation
             *
             * TODO: should we also evict the wasmInstance from cache, so it's reinstantaited
             * with the new memory for next time?
             */
            (err) => Resolved(assocPath(['Error'], err, {})),
            Resolved
          )
          .map(mergeOutput(Memory))
      )
      .toPromise()
}

if (!process.env.NO_WORKER) {
  const logger = createLogger(`ao-cu:${hostname()}:worker-${workerData.id}`)
  /**
   * Expose our worker api
   */
  worker({
    evaluate: evaluateWith({
      wasmModuleCache: createWasmModuleCache({ MAX_SIZE: workerData.WASM_MODULE_CACHE_MAX_SIZE }),
      wasmInstanceCache: createWasmInstanceCache({ MAX_SIZE: workerData.WASM_INSTANCE_CACHE_MAX_SIZE }),
      readWasmFile: readWasmFileWith({ DIR: workerData.WASM_BINARY_FILE_DIRECTORY }),
      writeWasmFile: writeWasmFileWith({ DIR: workerData.WASM_BINARY_FILE_DIRECTORY, logger }),
      streamTransactionData: streamTransactionDataWith({ fetch, GATEWAY_URL: workerData.GATEWAY_URL, logger }),
      bootstrapWasmInstance: (wasmModule, gasLimit, memoryLimit) => AoLoader(
        (info, receiveInstance) => WebAssembly.instantiate(wasmModule, info).then(receiveInstance),
        gasLimit,
        memoryLimit
      ),
      logger
    })
  })
}
