#!/usr/bin/env python3
from pyln.client import Plugin
import threading
import time

plugin = Plugin()
resolve_called = threading.Event()

@plugin.method("resolve")
def resolve(plugin):
    """Resolves all htlcs currently pending."""
    plugin.log('resolve called')
    resolve_called.set()
    return {}

@plugin.init()
def init(options, configuration, plugin, **kwargs):
    plugin.log("Plugin hodl_plugin.py initialized")

@plugin.async_hook("htlc_accepted")
def on_htlc_accepted(onion, htlc, plugin, request, **kwargs):
    plugin.log('on_htlc_accepted called')
    t = threading.Thread(target=hold_htlc, args=(plugin,request,))
    t.start()

def hold_htlc(plugin, request):
    plugin.log('hold_htlc called')
    resolve_called.wait()
    request.set_result({'result':'continue'})

plugin.run()
