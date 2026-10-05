# Testing

## Host tests

Unit tests live beside the code. Public-API tests against the mock backend are in `tests/mock.rs` and need the `mock` feature.

```bash
cargo test -p edgefirst-camera --features mock
cargo clippy -p edgefirst-camera --all-targets --features mock -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p edgefirst-camera --no-deps --features mock
```

These run in every CI lane:

- **Linux:** the Quick tier.
- **macOS and Windows:** the Full tier's `sdk-portable` job, triggered by the `ci:full` label.

## What is covered

| Area | Tests |
|---|---|
| Wall-clock conversion | `timestamp.rs`: bracket midpoint, retry of a wide bracket, forward and backward steps, pre-epoch times, conversion against the host clocks |
| Source strings | `builder.rs`: every form and the malformed ones |
| Mode naming | `enumerate.rs`: discrete sizes (OV5640 on ISI), stepwise ranges (vvcam), unknown rates, fractional rates |
| Errors | `error.rs`: rejection reason and slot, errno mapping, error sources |
| Slot ownership | `pool.rs`: a held slot is never released before its frame drops; release order; detached frames keep their memory and release nothing; a detached table hands out no frames; one slot cannot be framed twice; releases from other threads |
| Frame lifetime | `frame.rs` and `tests/mock.rs`: release on drop; starvation counted as drops; pool depths 2 to 8; a frame held across `stop()`/`start()` stays valid and requeues on drop; frames outliving a pool change and the camera |
| Buffers | `pool.rs`: each rejection reason (including a PBO tensor as `NotNativeHandle`) with its slot, count limits, contiguity and pitch rules. `tests/mock.rs`: caller pools, rejection and recovery through `take_buffers` and `set_buffers` |
| Controls | `tests/mock.rs`: applied, clamped and unsupported outcomes |

Device-backed tests arrive with the V4L2 backend. They run on the `vivid` virtual driver in CI and on the board lane, selected per the Testing and Validation page of the design.
