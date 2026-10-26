# UltraHLE Rebirth

<p align="center"><img src="./Picsart_26-10-20_10-43-04-335.jpg" alt="Senses" width="700"></p>
**UltraHLE Rebirth** is a community-maintained fork of [HyperHLE-Reborn](https://github.com/KlugKlugTG/HyperHLE-Reborn), itself based on [HyperHLE](https://github.com/HyperHLE/HyperHLE) and [touchHLE](https://github.com/touchHLE/touchHLE). The user-facing name of this project is **UltraHLE Rebirth**.

> **Why the rename?** This fork was renamed from HyperHLE-Reborn to UltraHLE Rebirth so it can be installed **side-by-side** with the main HyperHLE-Reborn app instead of replacing (overriding) it on a device. UltraHLE Rebirth has its own Android package identity and its own signing key:
>
> - Package name (application ID): `org.ultrahle.rebirth` (the main app uses `org.hyperhle.reborn`)
> - App label shown on the device: **UltraHLE Rebirth**
> - Signing certificate: a dedicated UltraHLE Rebirth key (`android/keystore/release.keystore`, SHA-256 fingerprint `45:64:D0:82:41:1F:DB:75:93:8F:E4:23:A5:6D:40:43:22:BA:BB:66:9C:26:7F:4F:65:CE:25:99:B7:3B:39:81`)
> - APK / release files: `UltraHLE-Rebirth-release.apk`, `UltraHLE-Rebirth_Android_AArch64.zip`
>
> Because both the package name and the signature differ, installing UltraHLE Rebirth will not overwrite, update, or conflict with an existing HyperHLE-Reborn install — both apps can coexist, each with its own data. Note that UltraHLE Rebirth therefore starts with its own fresh app data; it does not inherit the main app's files or settings.
>
> For CI signing, the keystore secrets were renamed from `HYPERHLE_*` to `ULTRAHLE_KEYSTORE_B64`, `ULTRAHLE_KEYSTORE_PASSWORD`, `ULTRAHLE_KEY_ALIAS` (and `ULTRAHLE_KEY_LINEAGE_B64`). If no `ULTRAHLE_*` secrets are configured, builds fall back to the committed UltraHLE Rebirth keystore.

UltraHLE Rebirth is a high-level emulator for early iPhone OS apps. It reimplements selected iOS frameworks on the host, allowing compatible apps to run without booting iOS. This fork is **Android-only** and focuses on Android/mobile usability, fullscreen and orientation handling, graphics compatibility, and app-specific fixes. It is not an official Apple product.

The repository does not distribute commercial apps or iOS firmware. Put an app bundle (`.app`) or IPA (`.ipa`) that you are entitled to use in `touchHLE_apps/`; it will appear in the app picker.

## Platforms and features

- Android-only builds (AArch64). The desktop/iOS code inherited from upstream is still in the tree, but only Android is built and released by this fork's CI.
- Virtual iPhone and iPad device profiles.
- Android touch input and fullscreen/orientation handling.
- Ongoing GLES, EAGL, Core Animation, audio, and iOS framework compatibility work.
- Download build artifacts from the UltraHLE Rebirth GitHub Actions workflow (`HyperHLE_release.yml` in this repository's Actions tab).

## Newly tested and working games

The titles below have been tested with the HyperHLE family (including upstream HyperHLE-Reborn) and are known to run. Compatibility may still vary with the app version, virtual device profile, host GPU/driver, and settings; this list does not guarantee that every feature or a full playthrough works. Also any update can break these games in list, report if it doesn't work

- N.O.V.A. 3 (Playable)
- Gangstar Vegas (Unplayable on adreno)
- Geometry Dash (Playable)
- Modern Combat 3 (Playable)
- Asphalt 7 (Playable)
- Turbo Dismount (Playable)
- Scarface (Playable)
- Zombie Safari (Playable)
- Real Racing 1 (Playable)
- Real Racing 3 (Playable)
- Silent Ops (Playable)
- Need For Speed: Most Wanted (Playable)
- Minecraft 0.14.2-0.16.2 (Playable)
- Oceanhorn (Playable)
- Asphalt 8 (Playable)
- GTA 3 (Ingame)
- Granny (Ingame)
- Talking Tom Hero Dash (Playable)
- My Talking Hank (Playable)
- And more more games

## Build and documentation

Build the Android app from the `android/` directory with Gradle (`assembleRelease`); CI does this on every push — see the `HyperHLE_release.yml` workflow. A desktop version inherited from upstream can still be built with Cargo:

```sh
cargo run --release -- path/to/app.app
```

Command-line options are documented in [`OPTIONS_HELP.txt`](OPTIONS_HELP.txt); notable compatibility changes are tracked in [`CHANGELOG.md`](CHANGELOG.md).

## Community

Join the [HyperHLE Discord server](https://discord.gg/ZpEkAV47H9) to discuss the project and contribute.

## Credits and license

UltraHLE Rebirth carries forward work from HyperHLE-Reborn, HyperHLE and touchHLE contributors and uses open-source libraries. It is licensed under the [Mozilla Public License 2.0](LICENSE); see the individual projects for their respective notices and licenses.
