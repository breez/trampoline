from io import BytesIO, StringIO
from pyln.testing.fixtures import *
from pyln.testing.utils import wait_for
from pyln.proto.onion import TlvPayload
import os

plugin_path = os.path.join(os.path.dirname(__file__), "../../target/debug/trampoline")

def test_trampoline_payment(node_factory):
    sender, router, recipient = node_factory.get_nodes(3)
    trampoline = node_factory.get_node(opts={'plugin': plugin_path})
    sender.fundwallet(2000000)
    trampoline.fundwallet(2000000)
    router.fundwallet(2000000)
    recipient.fundwallet(2000000)
    sender.rpc.connect(trampoline.info['id'], 'localhost', port=trampoline.port)
    sender.rpc.fundchannel(trampoline.info['id'], 1000000)
    trampoline.rpc.connect(router.info['id'], 'localhost', port=router.port)
    trampoline.rpc.fundchannel(router.info['id'], 1000000)
    router.rpc.connect(recipient.info['id'], 'localhost', port=recipient.port)
    router.rpc.fundchannel(recipient.info['id'], 1000000)
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in sender.rpc.listpeerchannels()['channels']), timeout=120)
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in trampoline.rpc.listpeerchannels()['channels']), timeout=120)
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in router.rpc.listpeerchannels()['channels']), timeout=120)

    def truncate_encode(i: int):
        """Encode a tu64 (or tu32 etc) value"""
        ret = struct.pack("!Q", i)
        while ret.startswith(b'\0'):
            ret = ret[1:]
        return ret

    blockheight = sender.rpc.getinfo()['blockheight']
    invoice = recipient.rpc.invoice(1000000, "trampoline", "trampoline")
    payload = TlvPayload()

    # amt_to_forward
    b = BytesIO()
    b.write(truncate_encode(1005000))
    payload.add_field(2, b.getvalue())

    # outgoing_cltv_value
    b = BytesIO()
    b.write(truncate_encode(blockheight + 600))
    payload.add_field(4, b.getvalue())

    # payment_data
    b = BytesIO()
    b.write(bytes.fromhex(invoice['payment_secret']))
    b.write(truncate_encode(1005000))
    payload.add_field(8, b.getvalue())

    # trampoline_invoice
    b = StringIO()
    b.write(invoice['bolt11'])
    payload.add_field(33001, b.getvalue())

    hops = [{
        "pubkey": trampoline.info['id'],
        "payload": payload.to_bytes().hex()
    }]
    first_hop = {
        "id": trampoline.info['id'],
        "amount_msat": 1005000,
        "delay": 600
    }
    onion = sender.rpc.createonion(hops=hops, assocdata=invoice['payment_hash'])
    sender.rpc.sendonion(onion=onion['onion'], first_hop=first_hop,
                     payment_hash=invoice['payment_hash'])
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
