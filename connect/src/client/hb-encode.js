import base64url from 'base64url'
import { Buffer } from 'buffer/index.js'

/**
 * polyfill in Browser
 */
if (!globalThis.Buffer) globalThis.Buffer = Buffer

/**
 * ******
 * HyperBEAM Http Encoding
 *
 * TODO: bundle into a package with
 *
 * - export encode()
 * - export encodeDataItem() to convert object
 * or ans104 to http message
 * - exported signers for both node and browser environments
 * (currently located in wallet.js modules)
 * ******
 */

/**
 * @param {ArrayBuffer} data
 */
async function sha256 (data) {
  return crypto.subtle.digest('SHA-256', data)
}

function partition (pred, arr) {
  return arr.reduce((acc, cur) => {
    acc[pred(cur) ? 0 : 1].push(cur)
    return acc
  },
  [[], []])
}

function isBytes (value) {
  return value instanceof ArrayBuffer ||
    ArrayBuffer.isView(value)
}

function isPojo (value) {
  return !isBytes(value) &&
    typeof value === 'object' &&
    value !== null
}

function hbEncodeValue (value) {
  if (isBytes(value)) {
    if (value.byteLength === 0) return hbEncodeValue('')
    return [undefined, value]
  }

  if (typeof value === 'string') {
    if (value.length === 0) return [undefined, 'empty-binary']
    return [undefined, value]
  }

  if (Array.isArray(value) && value.length === 0) {
    return ['empty-list', undefined]
  }

  if (typeof value === 'number') {
    if (!Number.isInteger(value)) return ['float', `${value}`]
    return ['integer', `${value}`]
  }

  if (typeof value === 'symbol') {
    return ['atom', value.description]
  }

  throw new Error(`Cannot encode value: ${value.toString()}`)
}

function store (fullK, curK, dest, [type, value]) {
  const [encoded, types] = dest
  if (type) types[curK] = type
  if (value) encoded[fullK] = value
  return dest
}

function hbEncode (obj, parent = '') {
  const [flattened, types] = Object.entries(obj)
    .reduce((acc, [key, value]) => {
      const flatK = (parent ? `${parent}/${key}` : key)
        .toLowerCase()

      // skip nullish values
      if (value == null) return acc

      // first/{idx}/name flatten array
      if (Array.isArray(value)) {
        if (value.length === 0) {
          return store(flatK, key, acc, hbEncodeValue(value))
        }
        value.forEach((v, i) =>
          Object.assign(acc[0], hbEncode(v, `${flatK}/${i}`))
        )
        return acc
      }

      // first/second flatten object
      if (isPojo(value)) {
        Object.assign(acc[0], hbEncode(value, flatK))
        return acc
      }

      // leaf encode value
      return store(flatK, key, acc, hbEncodeValue(value))
    }, [{}, {}])

  /**
   * Add the ao-types key for the specific layer,
   * as a structured dictionary
   */
  if (Object.keys(types).length) {
    const typesKey = (parent ? `${parent}/ao-types` : 'ao-types')
    flattened[typesKey] = Object.entries(types)
      .map(([key, value]) => `${key.toLowerCase()}=${value}`)
      .join(',')
  }

  return flattened
}

async function boundaryFrom (bodyParts = []) {
  const base = new Blob(
    bodyParts.flatMap((p, i, arr) =>
      i < arr.length - 1 ? [p, '\r\n'] : [p])
  )

  const hash = await sha256(await base.arrayBuffer())
  return base64url.encode(Buffer.from(hash))
}

/**
 * Encode the object as HyperBEAM HTTP multipart
 * message. Nested objects are flattened to a single
 * depth multipart
 */
export async function encode (obj = {}) {
  if (Object.keys(obj) === 0) return

  const flattened = hbEncode(obj)
  /**
   * Some values may be encoded into headers,
   * while others may be encoded into the body
   */
  const [bodyKeys, headerKeys] = partition(
    (key) => {
      if (key.includes('/')) return true
      const bytes = Buffer.from(flattened[key])
      /**
       * Anything larger than 4k goes into
       * the body
       */
      return bytes.byteLength > 4096
    },
    Object.keys(flattened).sort()
  )

  const h = new Headers()
  headerKeys.forEach((key) => h.append(key, flattened[key]))
  /**
   * Add headers that indicates and orders body-keys
   * for the purpose of determinstically reconstructing
   * content-digest on the server
   */
  // const bk = hbEncodeValue('body-keys', bodyKeys)
  // Object.keys(bk).forEach((key) => h.append(key, bk[key]))

  let body
  if (bodyKeys.length) {
    const bodyParts = await Promise.all(
      bodyKeys.map((name) => new Blob([
        `content-disposition: form-data;name="${name}"\r\n\r\n`,
        flattened[name]
      ]).arrayBuffer())
    )

    const boundary = await boundaryFrom(bodyParts)

    /**
     * Segment each part with the multipart boundary
     */
    const blobParts = bodyParts
      .flatMap((p) => [`--${boundary}\r\n`, p, '\r\n'])

    /**
     * Add the terminating boundary
     */
    blobParts.push(`--${boundary}--`)

    body = new Blob(blobParts)
    /**
     * calculate the content-digest
     */
    const contentDigest = await sha256(await body.arrayBuffer())
    const base64 = base64url.toBase64(base64url.encode(contentDigest))

    h.set('Content-Type', `multipart/form-data; boundary="${boundary}"`)
    h.append('Content-Digest', `sha-256=:${base64}:`)
  }

  return { headers: h, body }
}
