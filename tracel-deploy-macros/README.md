# tracel-deploy-macros

Proc macros for [`tracel-deploy`](https://crates.io/crates/tracel-deploy).

The `deploy_commands` attribute mirrors `tracel_xtask_macros::base_commands`:

```rust
#[tracel_deploy::deploy_commands(Deploy, handler = crate::commands::deploy::run)]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {
    Setup(SetupArgs),
}
```

`deploy_commands` is intentionally placed before `base_commands` because
`base_commands` reconstructs the enum and does not re-emit unknown outer
attribute macros.

See `tracel-deploy` for the runtime deploy builder, pre/post hooks, and the
cross-platform running-binary rotation implementation.
