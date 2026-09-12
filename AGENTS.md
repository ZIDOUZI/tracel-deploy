# AGENTS.md — tracel-deploy

## 发布流程（顺序不可颠倒）

**先推 GitHub，再发 crates.io。** 0.2.0 反着做过一次，结果已发布 crate 的
`.cargo_vcs_info.json` 记录了一个 GitHub 上不可达的提交哈希。

1. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --features service -- -D warnings`、
   `cargo test --workspace --features service`、`cargo test --workspace` 全绿。
2. 改版本与文档：根 `Cargo.toml` 的 `workspace.package.version`，
   `tracel-deploy/Cargo.toml` 里 `tracel-deploy-macros` 的 `version` 必须同步，
   并在 `CHANGELOG.md` 写好该版本条目。
3. **推送**：`jj git push --bookmark main --remote origin`，然后用
   `git ls-remote --heads origin` 确认远程 `main` 指向本次发布提交。
4. **发布**：先 `cargo publish -p tracel-deploy-macros`，再
   `cargo publish -p tracel-deploy`（后者按 path + version 依赖前者，顺序不能反）。
5. **发布后验证**：`cargo search tracel-deploy` 能看到新版本，并且在一个全新临时
   crate 里依赖该版本编译通过（验证 registry 解析，而不是只看 publish 输出）。

为什么必须先推：本仓库 `jj config` 打开了 `git.sign-on-push = true`，推送时会给
提交补签名并重写哈希（0.2.0 推送后 6 个提交的哈希全部变化）。先发布会让 crate 里的
vcs info 指向重写前的孤儿提交。

## 推送注意事项

- 本仓库配置了 `git.push = "blocked"`（防误推开关）。推送必须显式指定
  `jj git push --bookmark main --remote origin`；不要为了省事删除该配置。
- 需要「先验证再推送」的场景仍然适用：改动未经验证时不要把 `main` 推到 origin。

## 仓库约定

- `.scratch/` 是机器本地目录（ticket、消费方盘点、验证产物都在这里，不入库）。
  ticket 路径约定 `.scratch/<task>/issues/NN-slug.md`，文件里有
  `**Status:** ready-for-agent | done (wave N, 日期)`。
- 多 agent 并行时按文件独占分工，不要让两个 agent 同时改同一个源文件；
  收尾由主 agent 跑 `cargo fmt --all`，中途只对单文件做 `rustfmt --check`。
- Windows + PowerShell：直接用 PowerShell 命令；`.git` 在沙箱内只读，jj 的
  commit/push 等写操作需要提权执行。
