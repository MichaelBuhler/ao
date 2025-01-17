import { identity } from 'ramda'
import { of, fromPromise, Rejected } from 'hyper-async'

function writeDataItemWith ({ fetch, logger }) {
  return async ({ data, suUrl }) => {
    logger.info('SU URL: ', suUrl)
    return of(Buffer.from(data, 'base64'))
      .map(logger.info(`Forwarding message to SU ${suUrl}`))
      .chain(fromPromise((body) =>
        fetch(suUrl, {
          method: 'POST',
          headers: {
            'Content-Type': 'application/octet-stream',
            Accept: 'application/json'
          },
          body
        })
      ))
      .bimap(
        logger.info('Error while communicating with SU:'),
        identity
      )
      .bichain(
        (err) => Rejected(JSON.stringify(err)),
        fromPromise(async (res) => {
          if (!res?.ok) {
            const text = await res.text()
            throw new Error(`${res.status}: ${text}`)
          }
          return res.json()
        })
      )
      .map(logger.info('Successfully forwarded DataItem to SU'))
      .toPromise()
  }
}

function fetchSequencerProcessWith ({ logger }) {
  return async (processId, suUrl) => {
    logger(`${suUrl}/processes/${processId}`)

    return fetch(`${suUrl}/processes/${processId}`)
      .then(res => res.json())
      .then(res => res || {})
  }
}

export default {
  writeDataItemWith,
  fetchSequencerProcessWith
}
