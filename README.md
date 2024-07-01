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
--trampoline-cltv-delta <arg>                     The number of blocks between incoming payments and outgoing payments: this
                                                  needs to be enough to make sure that if we have to, we can close the
                                                  outgoing payment before the incoming, or redeem the incoming once the
                                                  outgoing is redeemed. (default: 34)
--trampoline-policy-cltv-delta <arg>              Cltv expiry delta for the trampoline routing policy. Any routes where the
                                                  total cltv delta is lower than this number will not be tried. (default: 576)
--trampoline-policy-fee-per-satoshi <arg>         This is the proportional fee to charge for every trampoline payment which
                                                  passes through. As percentages are too coarse, it's in millionths, so 10000
                                                  is 1%, 1000 is 0.1%. (default: 5000)
--trampoline-policy-fee-base <arg>                The base fee to charge for every trampoline payment which passes through.
                                                   (default: 0)
--trampoline-mpp-timeout <arg>                    Timeout in seconds before multipart htlcs that don't add up to the payment
                                                  amount are failed back to the sender. (default: 60)
--trampoline-no-self-route-hints                  If this flag is set, invoices where the current node is in an invoice route
                                                  hint are not supported. This can be useful if there are other important
                                                  plugins acting only on forwards. The trampoline plugin will 'receive' and
                                                  'pay', so has different dynamics.
```

## Testing
To run all tests call `make test`, or `make test -j2`

### Unit tests
To run only the unit tests call `make utest`.

### Integration tests

#### Prerequisites
In order to run the integration tests you need 
- python >= 3.8, < 4.0
- `virtualenv` (`python -m pip install --user virtualenv`)
- `lightningd` accessible through your `PATH`
- `bitcoind` and `bitcoin-cli` accessible through your `PATH`

#### Running integration tests
Call `make itest` to only run the integration tests. To run a single test, use
`PYTEST_OPTS="-k name_of_test" make itest`.

## Contributing
Contributions are welcome!
Make sure to run `make check` before committing, or `make check -j4` if you like
to be fast.