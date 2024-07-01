from pyln.testing.fixtures import *
from pyln.testing.utils import wait_for
import helpers

def test_force_close(node_factory):
    sender, trampoline, recipient = helpers.setup(node_factory, hodl_plugin=True, may_reconnect=False)
    invoice = recipient.rpc.invoice(500_000_000, "trampoline", "trampoline")
    helpers.send_onion(sender, trampoline, invoice, 502_500_000, 502_500_000)
    wait_for(lambda: len(recipient.rpc.listpeerchannels()['channels'][0]['htlcs']) > 0)

    # force close from sender
    sender.rpc.disconnect(trampoline.info['id'], force=True)
    sender.rpc.call("close", {
        "id": trampoline.info['id'],
        "unilateraltimeout": 1,
    })

    # Make sure everyone sees it onchain
    wait_for(lambda: sender.rpc.listpeerchannels(peer_id=trampoline.info['id'])['channels'][0]['state'] == 'AWAITING_UNILATERAL')
    sender.bitcoin.generate_block(1, wait_for_mempool=1)
    wait_for(lambda: trampoline.rpc.listpeerchannels(peer_id=sender.info['id'])['channels'][0]['state'] == 'ONCHAIN')
    
    # settle from recipient, this should make us claim the htlc on the incoming side.
    recipient.rpc.call("resolve", {})

    # Ensure we take the htlc tx
    rawtx, txid, blocks = trampoline.wait_for_onchaind_tx('THEIR_HTLC_FULFILL_TO_US', 'THEIR_UNILATERAL/THEIR_HTLC')
    assert blocks == 0
    tx = trampoline.bitcoin.rpc.decoderawtransaction(rawtx, True)

    # Take a little margin for the fee, but this should be the htlc tx basically.
    assert tx['vout'][0]['value'] > 0.00500000 and tx['vout'][0]['value'] < 0.00502500