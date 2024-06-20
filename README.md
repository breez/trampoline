## Trampoline plugin
This is a trampoline plugin for Core Lightning that allows you to facilitate
trampoline payments. Currently it is not using the trampoline protocol as
defined in [Trampoline Routing](https://github.com/lightning/bolts/pull/829) and
[Trampoline onion format](https://github.com/lightning/bolts/pull/836), but a
custom protocol.

## Installation
In order to install `trampoline` you have to compile from source.

### Prerequisites
- [Rust](https://www.rust-lang.org/tools/install) installation

### Compilation
Run `make release` in the root folder of this repository. The binary will be
`target/release/trampoline`.

### Running
Run cln with `--plugin=trampoline`

There's configuration options for the trampoline plugin that can to be passed
to core lightning on startup.

```
--trampoline-mpp-timeout <arg>                    Timeout in seconds before multipart htlcs that don't add up to the payment amount are failed back to the sender. (default: 60)
--trampoline-cltv-expiry-delta <arg>              Cltv expiry delta for the trampoline routing policy. Any routes where the total cltv delta is lower than this number will not be
                                                  tried. (default: 576)
--trampoline-no-self-route-hints                  If this flag is set, invoices where the current node is in an invoice route hint are not supported. This can be useful if there are
                                                  other important plugins acting only on forwards. The trampoline plugin will 'receive' and 'pay', so has different dynamics.
--trampoline-fee-base-msat <arg>                  Base fee in millisatoshi charged for trampoline payments. (default: 0)
--trampoline-fee-ppm <arg>                        Fee rate in parts per million charges for trampoline payments. (default: 5000)
```

## Testing

### Unit tests
To run the unit tests call `make utest`.

### Integration tests

#### Prerequisites
In order to run the integration tests you need 
- python >= 3.8, < 4.0
- `virtualenv` (`python -m pip install --user virtualenv`)

#### Running integration tests
Call `make itest` to run the integration tests.

## Contributing
Contributions are welcome!
Make sure to run `make check` before committing, or `make check -j4` if you like
to be fast.