from pyln.testing.fixtures import *
import helpers


def test_regular_forward(node_factory):
    sender, trampoline, recipient = helpers.setup(node_factory)
    invoice = recipient.rpc.invoice(1_000_000, "trampoline", "trampoline")
    sender.rpc.pay(invoice["bolt11"])
