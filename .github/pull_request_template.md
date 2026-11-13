## 变更说明

<!-- 做了什么、为什么；关联 issue/ADR。 -->

## 检查清单

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --no-fail-fast`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
- [ ] `python3 scripts/check-layering.py --baseline scripts/layering-baseline.json`
- [ ] `python3 scripts/check-tree.py` / `./scripts/check-docs-paths.sh`
- [ ] 公共 API 变更已更新 `docs/guides/migration/` 与 CHANGELOG
- [ ] 新模块遵守 `docs/architecture/decisions/0003-layering-rules.md`
