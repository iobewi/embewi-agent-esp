# config-space-manager

`config-space-manager` is a small `no_std` configuration-storage ownership layer for embedded systems.

A component claims an isolated opaque space:

```rust
let wifi = manager.claim("wifi", Budget::new(512))?;
```

If the backend can guarantee the requested reservation, the component receives a `ConfigSpace` capability. It owns the byte encoding inside that space:

```rust
let current = wifi.load().await?;
wifi.commit(serialized_wifi_config).await?;
```

The manager does **not** know what an SSID, certificate, token, GPIO, or controller URL is.

## Boundary

`config-space-manager` owns:

- unique space ownership;
- reservation/admission control;
- per-space maximum payloads;
- opaque replacement commits;
- generations;
- isolation through capability handles.

It deliberately does **not** own:

- component schemas or migrations;
- Wi-Fi/TLS/application policy;
- ESP flash/NVS mechanics;
- serialization formats;
- provisioning protocols.

## Capacity model

A logical payload byte is not assumed to equal one physical storage byte.

```text
Budget(max payload)
        ↓
reservation_units()
        ↓
backend-specific capacity accounting
```

This lets an ESP NVS backend reserve conservatively for page/entry overhead while a host/test backend can use a simple byte-for-byte model.

A successful claim is a boot-lifetime guarantee: later components cannot consume capacity already reserved for it.

## Storage model

Each component gets one opaque value, not a nested key/value database. That keeps the manager schema-agnostic and lets the persistence backend provide atomic whole-configuration replacement.

A component which wants fields such as `ssid/password` or `cert/key/ca` serializes them inside its own blob and owns any schema-version migration.

## Status

Initial API under active development. The first integration target is `embewi-agent-esp`, backed by the existing `esp-storage-manager`/NVS layer.
