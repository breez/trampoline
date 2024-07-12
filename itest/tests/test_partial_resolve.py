from pyln.testing.fixtures import *
from pyln.testing.utils import wait_for
import helpers


def connect_nodes(sender, trampoline, recipient):
    sender.openchannel(trampoline, 2_000_000)
    trampoline.openchannel(recipient, 900_000)
    trampoline.openchannel(recipient, 900_000)
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


def test_partial_resolve(node_factory):
    """Test whether the sender's payment succeeds if a trampoline payment partially resolves."""
    sender, trampoline, recipient = helpers.setup(
        node_factory, hodl_plugin=True, connect_nodes=connect_nodes
    )
    preimage = "8342775fce6e7c89422d54043079e14a594d4c0565dfe94831eb75d660355ab6"
    invoice = recipient.rpc.invoice(
        1_000_000_000, "trampoline", "trampoline", preimage=preimage
    )
    helpers.send_onion(
        sender, trampoline, invoice, 1_005_000_000, 1_005_000_000, 1_000_000_000
    )

    # Wait for all parts to arrive at the recipient
    wait_for(
        lambda: sum(
            sum((htlc["amount_msat"] for htlc in channel["htlcs"]))
            for channel in recipient.rpc.listpeerchannels()["channels"]
        )
        == 1_000_000_000
    )

    # Fail one part
    recipient.rpc.call(
        "resolve", {"index": 0, "result": {"result": "fail", "failure_message": "2002"}}
    )

    # Wait for the htlc to resolve
    wait_for(
        lambda: sum(
            sum((htlc["amount_msat"] for htlc in channel["htlcs"]))
            for channel in recipient.rpc.listpeerchannels()["channels"]
        )
        < 1_000_000_000
    )

    # Resolve the other parts successfully.
    recipient.rpc.call(
        "resolve",
        {"index": -1, "result": {"result": "resolve", "payment_key": preimage}},
    )

    # Should settle
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
    assert result["status"] == "complete"
