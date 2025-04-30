
import { connect } from '@permaweb/aoconnect'

const { dryrun } = connect({
    CU_URL: 'http://localhost:6363'
})

const process = 'PscuEMuK_bGAWfoLlHVtP9DJwHHnRt4_DcvwkdTtB1A'

// warmup the process
console.log('warming up process on CU...')
await dryrun({ process, data: 'warmup' })
console.log('done.')

console.log('starting real test...')
for (let i = 0 ; i < 100 ; i++ ) {
    console.log(`send: ${i}`)
    dryrun({ process }).then(() => {
        console.log(`done: ${i}`)
    })
    const delay = Math.random() * 180 + 20 // rand between 20 and 200
    await new Promise(resolve => setTimeout(resolve, delay))
}
console.log('done')
