from pyln.testing.fixtures import *
import helpers

def test_trampoline_payment(node_factory):
    sender, trampoline, recipient = helpers.setup(node_factory)
    invoice = recipient.rpc.invoice(1_000_000, "trampoline", "trampoline")
    helpers.send_onion(sender, trampoline, invoice, 1_005_000, 1_005_000)
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
