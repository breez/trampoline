import os
import struct
from io import BytesIO
from pyln.testing.utils import wait_for
from pyln.proto.onion import TlvPayload

plugin_path = os.path.join(os.path.dirname(__file__), "../../target/debug/trampoline")
hodl_plugin_path = os.path.join(os.path.dirname(__file__), "hodl_plugin.py")

def connect_nodes(sender, trampoline, recipient):
    sender.openchannel(trampoline, 1000000)
    trampoline.openchannel(recipient, 1000000)
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in sender.rpc.listpeerchannels()['channels']))
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in trampoline.rpc.listpeerchannels()['channels']))

def setup(node_factory, hodl_plugin=False, may_reconnect=False, connect_nodes=connect_nodes):
    sender_opts = {}
    recipient_opts = {}
    trampoline_opts = {'plugin': plugin_path, 'trampoline-mpp-timeout': '15'}
    if hodl_plugin:
        recipient_opts['plugin'] = hodl_plugin_path
    
    if may_reconnect:
        recipient_opts['may_reconnect'] = True
        sender_opts['may_reconnect'] = True

    sender, recipient = node_factory.get_nodes(2, [sender_opts, recipient_opts])
    trampoline = node_factory.get_node(options=trampoline_opts, start=False, may_reconnect=may_reconnect)
    trampoline.daemon.env['CLN_PLUGIN_LOG'] = 'cln_plugin=trace,cln_rpc=trace,cln_grpc=trace,trampoline=trace,debug'
    try:
        trampoline.start(True)
    except Exception:
        trampoline.daemon.stop()
        raise

    connect_nodes(sender, trampoline, recipient)
    return sender, trampoline, recipient

def send_onion(sender, trampoline, invoice, amount_msat, total_msat, delay=1008, partid=0, groupid=0):
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
    sender.rpc.call("sendonion", {
        "onion": onion['onion'],
        "first_hop": first_hop,
        "payment_hash": invoice['payment_hash'],
        "partid": partid,
        "groupid": groupid
    })
