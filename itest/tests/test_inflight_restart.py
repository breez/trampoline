from pyln.testing.fixtures import *
from pyln.testing.utils import wait_for
import helpers


def test_inflight_restart(node_factory):
    sender, trampoline, recipient = helpers.setup(
        node_factory, hodl_plugin=True, may_reconnect=True
    )
    invoice = recipient.rpc.invoice(1_000_000, "trampoline", "trampoline")

    helpers.send_onion(sender, trampoline, invoice, 1_005_000, 1_005_000)
    wait_for(lambda: len(recipient.rpc.listpeerchannels()["channels"][0]["htlcs"]) > 0)
    trampoline.restart()
    trampoline.connect(recipient)
    trampoline.connect(sender)
    wait_for(
        lambda: all(
            channel["state"] == "CHANNELD_NORMAL"
            for channel in trampoline.rpc.listpeerchannels()["channels"]
        )
    )

    recipient.rpc.call("resolve", {})
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
    assert result["status"] == "complete"
