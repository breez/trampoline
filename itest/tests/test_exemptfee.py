from pyln.testing.fixtures import *
import helpers


def test_exemptfee(node_factory):
    sender, trampoline, router, recipient = helpers.setup_with_router(node_factory)
    invoice = recipient.rpc.invoice(1_000, "trampoline", "trampoline")
    helpers.send_onion(sender, trampoline, invoice, 1_005, 1_005, 1_000)
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
    assert result["status"] == "complete"
