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
    wait_for(
        lambda: all(
            channel["state"] == "CHANNELD_NORMAL"
            for channel in sender.rpc.listpeerchannels()["channels"]
        )
    )
    wait_for(
        lambda: all(
            channel["state"] == "CHANNELD_NORMAL"
            for channel in trampoline.rpc.listpeerchannels()["channels"]
        )
    )


def setup(
    node_factory, hodl_plugin=False, may_reconnect=False, connect_nodes=connect_nodes
):
    sender_opts = {}
    recipient_opts = {}
    trampoline_opts = {"plugin": plugin_path, "trampoline-mpp-timeout": "15"}
    if hodl_plugin:
        recipient_opts["plugin"] = hodl_plugin_path

    if may_reconnect:
        recipient_opts["may_reconnect"] = True
        sender_opts["may_reconnect"] = True

    sender, recipient = node_factory.get_nodes(2, [sender_opts, recipient_opts])
    trampoline = node_factory.get_node(
        options=trampoline_opts, start=False, may_reconnect=may_reconnect
    )
    trampoline.daemon.env["CLN_PLUGIN_LOG"] = (
        "cln_plugin=trace,cln_rpc=trace,cln_grpc=trace,trampoline=trace,debug"
    )
    try:
        trampoline.start(True)
    except Exception:
        trampoline.daemon.stop()
        raise

    connect_nodes(sender, trampoline, recipient)
    return sender, trampoline, recipient


def send_onion(
    sender,
    trampoline,
    invoice,
    amount_msat,
    total_msat,
    invoice_amount_msat,
    delay=1008,
    partid=0,
    groupid=0,
):
    def truncate_encode(i: int):
        """Encode a tu64 (or tu32 etc) value"""
        ret = struct.pack("!Q", i)
        while ret.startswith(b"\0"):
            ret = ret[1:]
        return ret

    scid = sender.rpc.listpeerchannels(peer_id=trampoline.info["id"])["channels"][0][
        "short_channel_id"
    ]
    blockheight = sender.rpc.getinfo()["blockheight"]
    payment_metadata = TlvPayload()

    # trampoline_invoice
    b = BytesIO()
    b.write(invoice["bolt11"].encode())
    payment_metadata.add_field(33001, b.getvalue())

    # trampoline_amount
    b = BytesIO()
    b.write(truncate_encode(invoice_amount_msat))
    payment_metadata.add_field(33003, b.getvalue())

    sender.rpc.call(
        "sendpay",
        {
            "route": [
                {
                    "amount_msat": amount_msat,
                    "id": trampoline.info["id"],
                    "delay": delay,
                    "channel": scid,
                }
            ],
            "payment_hash": invoice["payment_hash"],
            "amount_msat": total_msat,
            "partid": partid,
            "groupid": groupid,
            "payment_metadata": payment_metadata.to_bytes(False).hex(),
        },
    )
