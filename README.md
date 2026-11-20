# rmp-bridge

**rmp-bridge** provides a zero-touch shadow runtime and real-time bidirectional synchronization bridge between **ReMakePlace** and **[Foundation](https://github.com/bloooowfish/Foundation)** (the FFXIV Housing Studio simulator), orchestrated by **[Foundation Studio](https://github.com/bloooowfish/Foundation-Studio)** or via the standalone CLI.

Your original ReMakePlace installation (such as `D:\ReMakePlace_Latest`) remains untouched: asset/engine directories are mounted via NTFS junctions while `MakePlace\Save` and `config.json` are isolated in `runtime/`.

## Components

- **`scripts/prepare-shadow.ps1`**: Creates the shadow runtime under `runtime/`, automatically downloads and SHA-256-verifies official RE-UE4SS (`v4.0.0`) into `vendor/ue4ss-v4.0.0/` on first run if missing, links `mods/`, and keeps `MakePlace\Save` as an isolated copy so saves and autosaves never write into the original ReMakePlace folder.
- **`mods/FoundationBridge/Scripts/main.lua`**: RE-UE4SS Lua mod polling active target drags (`40ms`), hooking ReMakePlace commit operations, dumping headless snapshots, and applying remote move/dye/reload commands received over the file mailbox (`runtime/bridge_ipc/`).
- **`mods/FoundationDumper/Scripts/main.lua`**: Optional one-shot UE5 reflection and header dumper (`scripts/run-dumper.ps1`).
- **`relay/` (`rmp-bridge-relay`)**: Pure Rust library crate and standalone CLI binary connecting the file mailbox to Foundation's Housing API/SSE (`:21064/api/v1/housing/*`), featuring UE5 cm ↔ FFXIV meter coordinate conversion, sticky `ActorSlotBinder` slot matching, and strict echo suppression (`source: "rmp"` & revision tracking).

## Usage

**Via [Foundation Studio](https://github.com/bloooowfish/Foundation-Studio) (recommended):**
Configure your ReMakePlace path in **Foundation Studio** and click **ReMakePlace Live Sync** — it runs `prepare-shadow.ps1`, launches the shadow ReMakePlace client, and hosts `rmp-bridge-relay` in-process.

**Standalone CLI:**

```powershell
# Prepare shadow runtime + build relay + launch both
.\scripts\launch-rmp-bridge.ps1 -SourceDir "D:\ReMakePlace_Latest"
```

```powershell
# Or test/run relay directly
cargo test --manifest-path .\relay\Cargo.toml
cargo run --manifest-path .\relay\Cargo.toml -- --api-url "http://127.0.0.1:21064" --ipc-dir ".\runtime\bridge_ipc"
```

## Credits & Acknowledgements

- **MakePlace & [ReMakePlace](https://github.com/RemakePlace/app)** — originally created by **jawslouis** and maintained by the ReMakePlace team.
- **[RE-UE4SS](https://github.com/UE4SS-RE/RE-UE4SS)** (MIT License, Copyright (c) 2022 Narknon & contributors) — Unreal Engine scripting/hooking runtime, downloaded from official releases on first run rather than committed here.
- **[Kawari](https://codeberg.org/redstrate/Kawari)** by **redstrate** and Kawari contributors — upstream FFXIV server emulator underlying the [Foundation](https://github.com/bloooowfish/Foundation) housing fork.

## License & Legal Notices

- **License**: `rmp-bridge` (PowerShell shadow/launch scripts, RE-UE4SS Lua mods, and `rmp-bridge-relay`) is licensed under the **[GNU General Public License v3.0 or later (GPL-3.0-or-later)](LICENSE)**.
- **RE-UE4SS Third-Party Notice**: RE-UE4SS (`v4.0.0`) is **not** distributed in this repository (`vendor/` is gitignored). On first run, `scripts/prepare-shadow.ps1` downloads the official `UE4SS_v4.0.0.zip` release from GitHub and verifies its SHA-256 digest (`251AD0D1381A241750E036F5918CBF2B5D7EC58EBC608CB2DD09D459D0C81E45`); RE-UE4SS itself remains under the **[MIT License](https://github.com/UE4SS-RE/RE-UE4SS/blob/main/LICENSE)** (`Copyright (c) 2022 Narknon & contributors`).
- **Third-Party Software & Non-Affiliation**:
  - `rmp-bridge` does **not** redistribute MakePlace, ReMakePlace, FINAL FANTASY XIV game files, or proprietary third-party binaries, and operates strictly against a user-supplied local ReMakePlace installation via junctions and isolated shadow saves without modifying original files.
  - *FINAL FANTASY* and *FINAL FANTASY XIV* are registered trademarks of Square Enix Co., Ltd. `rmp-bridge` is an independent, non-commercial community tool and is **not affiliated with, endorsed by, or supported by** Square Enix, MakePlace / ReMakePlace, RE-UE4SS, or upstream Kawari.
- **Repository Hygiene**: Downloaded RE-UE4SS binaries (`vendor/`), generated UE5 reflection dumps (`dumps/`), the isolated shadow runtime (`runtime/`), and Rust build artifacts (`relay/target/`) are excluded via [`.gitignore`](.gitignore).
