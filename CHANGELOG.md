# Changelog

## [0.6.0](https://github.com/wynxing/colab-playground/compare/v0.5.1...v0.6.0) (2026-11-25)


### Added

* **app:** let an agent read the app through a local endpoint ([aa34088](https://github.com/wynxing/colab-playground/commit/aa34088a5c98986292465a005a2b6820c3af9d46))
* **app:** let an agent drive the app, asking before it spends ([d982c8b](https://github.com/wynxing/colab-playground/commit/d982c8b5beba380cc83fd4bd0762f23602c3b50f))
* **app:** keep a log, and hold an agent's call for an hour ([1facaf4](https://github.com/wynxing/colab-playground/commit/1facaf48dd8ccc7818ddf887a4cde26296c44d6c))
* **app:** run_watch — the agent waits, and the app tells it when something happens ([5c3a81a](https://github.com/wynxing/colab-playground/commit/5c3a81a82e332c42adfb881f26f74f068e031a66))


### Fixed

* **app:** start a session an agent left unnamed, and find a run's files ([0124478](https://github.com/wynxing/colab-playground/commit/01244781d4400cade02a6233a1f92f47f2b0237a))
* **app:** answer an agent's plugin_run at a stage, not at the end of the run ([43efbb9](https://github.com/wynxing/colab-playground/commit/43efbb9755481906256e827abc0245342db68a73))
* **app:** say where a save looked, and never leave a call unanswered ([279017e](https://github.com/wynxing/colab-playground/commit/279017ed933c086a1affa186557146932930ffa8))
* **core:** hand the CLI a path it can write, so a download lands where it is asked ([14c5a21](https://github.com/wynxing/colab-playground/commit/14c5a21f7a9b25106dbffac084dc5dced71c6a65))


## [0.5.1](https://github.com/wynxing/colab-playground/compare/v0.5.0...v0.5.1) (2026-11-24)


### Changed

* sign the downloads, so an installed copy can update itself ([d615980](https://github.com/wynxing/colab-playground/commit/d6159806e6670458a62bc42aab1a8040a2238910))
* name Wynn in the executable's properties ([ca6a93e](https://github.com/wynxing/colab-playground/commit/ca6a93ef9f88f39e9ebfb6d744b0f203e5e4c0f9))


## [0.5.0](https://github.com/wynxing/colab-playground/compare/v0.4.0...v0.5.0) (2026-11-24)


### Added

* **app:** draw a run's workflow as a graph ([cbcb3c3](https://github.com/wynxing/colab-playground/commit/cbcb3c30dd6fafdad199e2d24dd97b455be554b5))
* **app:** say how to drive a ComfyUI run with an agent ([9c40c18](https://github.com/wynxing/colab-playground/commit/9c40c18be5d43f22b69367dda19844a2a05e8a78))
* **app:** show what a ComfyUI run produced ([717439c](https://github.com/wynxing/colab-playground/commit/717439c52596634ae77f37170fd6428de102d042))
* **app:** show what the chosen hardware burns per hour when starting a session ([3b5bcd3](https://github.com/wynxing/colab-playground/commit/3b5bcd3028793e5b9d5886823c8c31d804d593c5))
* **core:** bundle a ComfyUI plugin ([5f71f2a](https://github.com/wynxing/colab-playground/commit/5f71f2a9729bf6ef9fa761f4a886bca62af525c7))
* keep reading a live run, and save what it produced ([f706fc2](https://github.com/wynxing/colab-playground/commit/f706fc25d27cb086a48497c2e3293f6e6e85c05f))


### Changed

* **core:** write the session ledger once per usage round ([146ce3a](https://github.com/wynxing/colab-playground/commit/146ce3a301d907044dac3cdc7221ff60c3b3e4ae))


### Fixed

* **app:** name a new session after the hardware chosen in the form, not the default ([bb17287](https://github.com/wynxing/colab-playground/commit/bb17287ddc27a621f165b5f42a970002613c50e8))
* **core:** cover a ComfyUI worker's failure paths and clean up harder ([d3c2d4d](https://github.com/wynxing/colab-playground/commit/d3c2d4ddb43f7358c4066de2e50877a5c3cdeda4))
* **core:** cover the real install path of the ComfyUI worker ([57ab443](https://github.com/wynxing/colab-playground/commit/57ab443a9b256a4cedb2c53fcdf900e994770ba4))
* **core:** keep the ComfyUI worker's subprocess helper from being shadowed ([dbfac2e](https://github.com/wynxing/colab-playground/commit/dbfac2ea9c002e96cb7e46e25c414a2fdb8e0cea))
* **core:** leave a server that this worker does not own alone ([4b5b910](https://github.com/wynxing/colab-playground/commit/4b5b910fe628218d20d299f69c37518c19a5cddb))
* **core:** load the all-in-one Flux checkpoint and stop the server after a failed run ([499009c](https://github.com/wynxing/colab-playground/commit/499009c5e1e97e7dba115304d8917d01cec8b531))


## [0.4.0](https://github.com/scarletkc/colab-playground/compare/v0.3.0...v0.4.0) (2026-11-20)


### Added

* **app:** ask for a GitHub star or feedback once the app has proved useful ([#37](https://github.com/scarletkc/colab-playground/issues/37)) ([77e4f37](https://github.com/scarletkc/colab-playground/commit/77e4f37c614593ab7ea60165894fa4a5e21aff83))


### Fixed

* **app:** ask before quitting when the window is closed while the list of runtimes is refreshing ([#47](https://github.com/scarletkc/colab-playground/issues/47)) ([2739f98](https://github.com/scarletkc/colab-playground/commit/2739f98faaa89bdba0d8b2ee1006d5939c2e6e8f))
* **app:** keep a plugin's secret inputs in the form when Deploy is rejected ([#58](https://github.com/scarletkc/colab-playground/issues/58)) ([9994db1](https://github.com/scarletkc/colab-playground/commit/9994db1c1a2100acad4fa9248f27057378fc9eb1))
* **app:** keep port forwards open when an account switch cannot start, stop a plugin check when its dialog closes, and smaller fixes from the audit ([#61](https://github.com/scarletkc/colab-playground/issues/61)) ([3a1a6a1](https://github.com/scarletkc/colab-playground/commit/3a1a6a1914542b16fb76910efd63af4e356fbc87))
* **app:** keep the list of runtimes from showing a stopped runtime, or missing a new one, after a refresh that began earlier ([#48](https://github.com/scarletkc/colab-playground/issues/48)) ([9e3b459](https://github.com/scarletkc/colab-playground/commit/9e3b4593cd02199dbb65f283313ca7f88744d8bb))
* **app:** keep the newest 2,000 lines of a plugin run's log so a chatty script does not slow the window down ([#62](https://github.com/scarletkc/colab-playground/issues/62)) ([ccd8956](https://github.com/scarletkc/colab-playground/commit/ccd8956fa1d270ae65cd0f2ca4910b42dccf5cbf))
* **core:** keep a free port from being reported as in use when a forward starts or reconnects ([#65](https://github.com/scarletkc/colab-playground/issues/65)) ([8a6c51e](https://github.com/scarletkc/colab-playground/commit/8a6c51ea3d644a71be8e12618b4308b256596c24))
* **core:** keep a plugin run's log from falling behind a script that writes a lot, and show its last lines when the run ends ([#57](https://github.com/scarletkc/colab-playground/issues/57)) ([e319e34](https://github.com/scarletkc/colab-playground/commit/e319e342c4c72433e383e3ec4a5969aeafc0cb62))
* **core:** let the spend cap stop a session whose burn rate showed late, and keep a usage poll from switching the account back ([#50](https://github.com/scarletkc/colab-playground/issues/50)) ([aa5d282](https://github.com/scarletkc/colab-playground/commit/aa5d282715ce05217e6c6b9a6f0bc6fe989ea132))
* **core:** list a model's GGUF files when a file name has non-ASCII characters ([#53](https://github.com/scarletkc/colab-playground/issues/53)) ([687f85b](https://github.com/scarletkc/colab-playground/commit/687f85ba0f84ff5d9fd7841e9c0e188c91958bf7))
* **core:** remove the copy staged for a plugin preview when the install dialog closes or the install fails, and a Git checkout that did not finish ([#55](https://github.com/scarletkc/colab-playground/issues/55)) ([12db790](https://github.com/scarletkc/colab-playground/commit/12db790b443138cdffc7d5a7a5ecb3bcd68d093d))
* **core:** stop retrying a port forward that never connects, end ssh when the app is killed, and catch a local port taken on IPv6 ([#42](https://github.com/scarletkc/colab-playground/issues/42)) ([805cb38](https://github.com/scarletkc/colab-playground/commit/805cb3875c4c59b46261d56835ba54b1c724adde))
* keep a second Extend from being lost when the list of runtimes has not refreshed yet ([#54](https://github.com/scarletkc/colab-playground/issues/54)) ([b8ae68e](https://github.com/scarletkc/colab-playground/commit/b8ae68e9cfde704e7809f3d8eebf47ee6534f69c))
* use the Colab CLI path from Settings, retry a failed update check after an hour and fetch usage once per round ([#44](https://github.com/scarletkc/colab-playground/issues/44)) ([9b57ce8](https://github.com/scarletkc/colab-playground/commit/9b57ce8f2a5436640de98d4bc39bc7d6e07d32f4))

## [0.3.0](https://github.com/scarletkc/colab-playground/compare/v0.2.0...v0.3.0) (2026-11-19)


### Added

* download, check and install updates from inside the app ([#41](https://github.com/scarletkc/colab-playground/issues/41)) ([cfd98c2](https://github.com/scarletkc/colab-playground/commit/cfd98c2e6781e97a42cca35fa976d30a27f51a70))


### Changed

* name the downloads without the version ([cfd98c2](https://github.com/scarletkc/colab-playground/commit/cfd98c2e6781e97a42cca35fa976d30a27f51a70))


### Fixed

* **core:** end a session whose runtime was never recorded once nothing running could be it ([#38](https://github.com/scarletkc/colab-playground/issues/38)) ([9e5eb7e](https://github.com/scarletkc/colab-playground/commit/9e5eb7e34b7b1f8eb4800e66a1cf6243bf0ea6f7))

## [0.2.0](https://github.com/scarletkc/colab-playground/compare/v0.1.0...v0.2.0) (2026-11-18)


### Added

* **app:** add Japanese and Korean as display languages ([#19](https://github.com/scarletkc/colab-playground/issues/19)) ([def0fa8](https://github.com/scarletkc/colab-playground/commit/def0fa8049c636d51400b47e9a421c785c451a60))
* **app:** say how long Colab has not answered a request for a runtime ([#24](https://github.com/scarletkc/colab-playground/issues/24)) ([0f6c5f0](https://github.com/scarletkc/colab-playground/commit/0f6c5f029fb7d2f39e0433379d1db9987cbdd4f4))


### Changed

* **app:** show no app mark or name beside the traffic lights on macOS ([#11](https://github.com/scarletkc/colab-playground/issues/11)) ([2583758](https://github.com/scarletkc/colab-playground/commit/2583758f3934a00ad5d58ed42e4c00aa8664c0f4))


### Fixed

* **app:** give the New session and Forward dialogs a scrollbar and the window's height ([#18](https://github.com/scarletkc/colab-playground/issues/18)) ([72e6009](https://github.com/scarletkc/colab-playground/commit/72e60097f291a0a08be7735c86dfee6894c494e6))
* **app:** keep the billing title when only some accounts have unknown runtime ownership ([#31](https://github.com/scarletkc/colab-playground/issues/31)) ([17a099d](https://github.com/scarletkc/colab-playground/commit/17a099d1f7555b6e3519e511aabc2034b3350e19))
* **app:** list plugins before the Colab CLI is installed ([#10](https://github.com/scarletkc/colab-playground/issues/10)) ([778c5f9](https://github.com/scarletkc/colab-playground/commit/778c5f94caba6602023a876316ee5684f073472c))
* **app:** mark the unmanaged-runtime warning as a last reading when usage is stale ([#26](https://github.com/scarletkc/colab-playground/issues/26)) ([bc23ecd](https://github.com/scarletkc/colab-playground/commit/bc23ecd6e11ce265ecad2ee62b91ca0857deb3a9))
* **app:** say when the runtimes left behind by an account switch are unknown ([#20](https://github.com/scarletkc/colab-playground/issues/20)) ([2cdc26c](https://github.com/scarletkc/colab-playground/commit/2cdc26c8cfebc7391de54cfe38ed65b4ac3ccf7a))
* **core:** keep WSL and its session bus up so the keyring is reachable and stays unlocked ([#16](https://github.com/scarletkc/colab-playground/issues/16)) ([d7a0386](https://github.com/scarletkc/colab-playground/commit/d7a0386e80a913feb6c0a4baa87a2e4c6a187caf))
* **core:** let Cancel and Stop all end a runtime request Colab never answers ([#21](https://github.com/scarletkc/colab-playground/issues/21)) ([586e26a](https://github.com/scarletkc/colab-playground/commit/586e26a66cd57e6d843027b99b20b63a83c1c5db))
* **core:** reconcile ended sessions before counting unmanaged runtimes ([#27](https://github.com/scarletkc/colab-playground/issues/27)) ([7048695](https://github.com/scarletkc/colab-playground/commit/704869572c0f9249897eabb613e5f79796c2640a))
* **core:** show wsl.exe messages legibly and stop treating them as a WSL distribution ([#22](https://github.com/scarletkc/colab-playground/issues/22)) ([91b139a](https://github.com/scarletkc/colab-playground/commit/91b139ae092846d00a9364dc27fa1c644904598e)), closes [#14](https://github.com/scarletkc/colab-playground/issues/14)

## 0.1.0 (2026-11-17)


### Added

* initial release ([6701a59](https://github.com/scarletkc/colab-playground/commit/6701a59889884510a97f4e17ae7b03a5416955f5))
