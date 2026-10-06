# AGENTS.md

agent-traces is a native CLI that reads coding-agent session stores. Build, test, and command reference: `README.md`. Store formats: `docs/stores.md`.

## Rollout

A merge reaches no host until a release is built and installed on each one. Run these steps in order after every merge that changes the binary. A step that does not apply to the change is skipped, and the receipt says why. Each step is done when its check passes.

1. **Version and merge.** The PR bumps `version` in `Cargo.toml` (and `Cargo.lock` through `cargo build`) and names it in its title, as in `since: select by last activity (0.2.3)`. `gh pr merge <PR> -R royalaid/agent-traces --squash --delete-branch`. Check: `gh pr view <PR> -R royalaid/agent-traces --json state,mergeCommit --jq '.state, .mergeCommit.oid'` prints `MERGED` and the merge SHA.
2. **Tag.** `git tag vX.Y.Z <merge SHA> && git push origin vX.Y.Z`. Check: `git ls-remote --tags origin vX.Y.Z` prints the merge SHA.
3. **Build the archives.** GitHub Actions has started no run on this repo since 2026-09-28, so the archives are built by hand from a clean detached checkout of the tag (`git worktree add --detach ../agent-traces-vX.Y.Z vX.Y.Z`). `scripts/package.py` lays each archive out as `.github/workflows/release.yml` does and writes its LF `.sha256` sidecar into `dist/`.
   - On the Mac, build the two macOS targets and the Windows one (`cargo-xwin` cross-builds MSVC and downloads the CRT into its own cache):
     ```sh
     cargo build --locked --release --target aarch64-apple-darwin
     cargo build --locked --release --target x86_64-apple-darwin
     cargo xwin build --locked --release --target x86_64-pc-windows-msvc
     python3 scripts/package.py aarch64-apple-darwin
     python3 scripts/package.py x86_64-apple-darwin
     python3 scripts/package.py x86_64-pc-windows-msvc .exe
     uvx --with jsonschema==4.26.0 python tests/parity.py --schema --binary target/aarch64-apple-darwin/release/agent-traces
     ```
     The Windows binary must be PE32+ x86-64 (`file`) and must not import `VCRUNTIME140.dll` (`strings … | grep -i vcruntime` prints nothing); `.cargo/config.toml` sets the static CRT.
   - The Linux archive is built in WSL (`desktop-o91444g-wsl`): hand it to `codex@agent-traces@desktop-o91444g-wsl` over Telepathica with these commands. A plain `cargo build` of a musl target fails where no musl C compiler is installed, so it uses `cargo zigbuild`, which is installed there with Zig at `~/.local/zig` (not on PATH by default):
     ```sh
     export PATH="$HOME/.local/zig:$PATH"
     cargo zigbuild --locked --release --target x86_64-unknown-linux-musl
     python3 scripts/package.py x86_64-unknown-linux-musl
     uvx --with jsonschema==4.26.0 python tests/parity.py --schema --binary target/x86_64-unknown-linux-musl/release/agent-traces
     gh release upload vX.Y.Z -R royalaid/agent-traces dist/agent-traces-x86_64-unknown-linux-musl.tar.gz*
     ```
   Check: parity passes on each host that ran it.
4. **Release.** From the Mac's tag checkout:
   ```sh
   gh release create vX.Y.Z -R royalaid/agent-traces --verify-tag --title "agent-traces X.Y.Z" \
     --notes "<what changed, one paragraph>" dist/*.tar.gz dist/*.zip dist/*.sha256
   ```
   Check: `gh release view vX.Y.Z -R royalaid/agent-traces --json assets --jq '.assets[].name'` lists four archives and four `.sha256` files once WSL's upload lands.
5. **Install on each host.** Download, verify, and install the host's archive as `README.md`, "Install a release", describes. Installed paths: the Mac `~/.local/bin/agent-traces`; the PC `C:\Users\gwmai\.local\bin\agent-traces.exe`; WSL `~/.local/bin/agent-traces`. Hand the PC and WSL installs to `codex@agent-traces@desktop-o91444g` and `codex@agent-traces@desktop-o91444g-wsl`. On the PC, also run parity against the installed `.exe`, since the Mac cannot run it. Check, on each host: `agent-traces --version` prints `agent-traces X.Y.Z` and `agent-traces where` lists that host's stores.
6. **Docs and dependent skills.** The skill lives in the skills repo: `~/git/skills/agent-traces/` (its `references/native-cli.md` names the minimum release that has the features the skill relies on; raise it when the skill starts relying on this one). Update it for new commands, flags, or output, and check the skills that call the CLI: `grep -rl agent-traces ~/git/skills --include='*.md' --exclude-dir=docs`. Then run the skills repo's Rollout section, which carries the change to the apcx-agent-workspace mirror. Telepathica's `telepathica setup` checks for agent-traces on PATH; a renamed command needs a matching change in `~/git/agent-mailbox`. Check: each hit was read and still holds, or its fix rolled out.
7. **chezmoi.** Nothing chezmoi manages names agent-traces today. Check: `grep -n agent-traces ~/AGENTS.md ~/.claude/CLAUDE.md` prints nothing; if it does, follow the Rollout section of `~/.local/share/chezmoi/AGENTS.md`.

Finish with a receipt: each step marked done with its check output, skipped with the reason, or blocked with the failing output.
