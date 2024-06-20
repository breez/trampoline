from pyln.testing.fixtures import *
from helpers import send_onion
from pyln.testing.utils import wait_for
plugin_path = os.path.join(os.path.dirname(__file__), "../../target/debug/trampoline")

@pytest.mark.timeout(60)
def test_trampoline_payment(node_factory):
    sender, recipient = node_factory.get_nodes(2)
    trampoline = node_factory.get_node(options={'plugin': plugin_path}, start=False)
    trampoline.daemon.env['CLN_PLUGIN_LOG'] = 'cln_plugin=trace,cln_rpc=trace,cln_grpc=trace,trampoline=trace,debug'
    try:
        trampoline.start(True)
    except Exception:
        trampoline.daemon.stop()
        raise
    sender.openchannel(trampoline, 1000000)
    trampoline.openchannel(recipient, 1000000)
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in sender.rpc.listpeerchannels()['channels']))
    wait_for(lambda: all(channel['state'] == 'CHANNELD_NORMAL' for channel in trampoline.rpc.listpeerchannels()['channels']))

    invoice = recipient.rpc.invoice(1_000_000, "trampoline", "trampoline")
    send_onion(sender, trampoline, invoice, 1_005_000, 1_005_000)
    result = sender.rpc.waitsendpay(invoice["payment_hash"])
