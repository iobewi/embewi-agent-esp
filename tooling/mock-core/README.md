# mock-core

Mock "Core" server used for the heartbeat keep-alive and log-stream
hardware gates (`src/heartbeat.rs`, `src/log_stream.rs`): the device
opens an outbound TLS connection to it exactly like it would to the real
Core, so `ctrl_url`/CA behavior can be exercised without one.

## Usage

```sh
tooling/mock-core/generate-certs.sh <device-reachable-ip-or-host>
python3 tooling/mock-core/mock_core.py \
    --cert tooling/mock-core/server.pem --key tooling/mock-core/server.key
```

Then push the CA to the device and point it here:

```sh
scripts/test-api.sh <device-url> <token> push-ca tooling/mock-core/ca.pem
```

`ctrl_url` itself is set once during initial provisioning (Improv Serial
or the one-shot HTTP form) and can't be changed over the API afterwards
-- point it at wherever this mock will actually run *before* locking the
device, or reprovision from scratch (full erase) to change it.

See `mock_core.py --help` for `--close-after-ws-accept` (simulates the
Core accepting a log-stream WS upgrade then immediately closing it) and
other scenario flags.

## Certs

`generate-certs.sh` always produces a matching CA + leaf pair (both EC
P-256 -- this firmware's MbedTLS build has no RSA support, see the repo
root `Cargo.toml`). None of the generated files are committed
(`*.pem`/`*.key` are gitignored); regenerate them here whenever needed
rather than reusing a stray copy from somewhere else. A leaf cert is not
a CA even when nothing complains until you try to use it as one --
that's the specific mistake this script exists to make impossible.
