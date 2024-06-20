import os
import struct
from io import BytesIO
from pyln.testing.fixtures import *
from pyln.testing.utils import wait_for
from pyln.proto.onion import TlvPayload

plugin_path = os.path.join(os.path.dirname(__file__), "../../target/debug/trampoline")

def send_onion(sender, trampoline, invoice, amount_msat, total_msat, delay=576):
    def truncate_encode(i: int):
        """Encode a tu64 (or tu32 etc) value"""
        ret = struct.pack("!Q", i)
        while ret.startswith(b'\0'):
            ret = ret[1:]
        return ret

    blockheight = sender.rpc.getinfo()['blockheight']
    payload = TlvPayload()

    # amt_to_forward
    b = BytesIO()
    b.write(truncate_encode(amount_msat))
    payload.add_field(2, b.getvalue())

    # outgoing_cltv_value
    b = BytesIO()
    b.write(truncate_encode(blockheight + delay))
    payload.add_field(4, b.getvalue())

    # payment_data
    b = BytesIO()
    b.write(bytes.fromhex(invoice['payment_secret']))
    b.write(truncate_encode(total_msat))
    payload.add_field(8, b.getvalue())

    # trampoline_invoice
    b = BytesIO()
    b.write(invoice['bolt11'].encode())
    payload.add_field(33001, b.getvalue())

    hops = [{
        "pubkey": trampoline.info['id'],
        "payload": payload.to_bytes().hex()
    }]
    first_hop = {
        "id": trampoline.info['id'],
        "amount_msat": amount_msat,
        "delay": delay
    }
    onion = sender.rpc.createonion(hops=hops, assocdata=invoice['payment_hash'])
    sender.rpc.sendonion(onion=onion['onion'], first_hop=first_hop,
                     payment_hash=invoice['payment_hash'])
