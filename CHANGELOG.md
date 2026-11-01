# Changelog

## [0.9.3](https://github.com/ifdotpy/layr/compare/v0.9.2...v0.9.3) (2026-11-01)


### Bug Fixes

* Keep staged, cache and xattr data safe; faster saves; line move ([#100](https://github.com/ifdotpy/layr/issues/100)) ([5278335](https://github.com/ifdotpy/layr/commit/5278335e96d2a31ae4a4cfb1f76691eec3b15c0a))

## [0.9.2](https://github.com/ifdotpy/layr/compare/v0.9.1...v0.9.2) (2026-10-31)


### Bug Fixes

* **retest:** link instrumented binaries like the plain build ([#98](https://github.com/ifdotpy/layr/issues/98)) ([10f3002](https://github.com/ifdotpy/layr/commit/10f30029da148f18b44335b1b1364644b756a5e5))

## [0.9.1](https://github.com/ifdotpy/layr/compare/v0.9.0...v0.9.1) (2026-10-31)


### Bug Fixes

* **fs:** Same change and cache rules on every backend, refuse Unicode name collisions ([#87](https://github.com/ifdotpy/layr/issues/87)) ([6fe5647](https://github.com/ifdotpy/layr/commit/6fe5647ac56db872ad26f40151a16e3531f0c6e2))


### Performance

* Less work under the store lock and fewer full walks ([#84](https://github.com/ifdotpy/layr/issues/84)) ([ee50ecc](https://github.com/ifdotpy/layr/commit/ee50eccfb19c0cd93f812df50a7b97b4dc84976c))

## [0.9.0](https://github.com/ifdotpy/layr/compare/v0.8.0...v0.9.0) (2026-10-31)


### Features

* **gc:** share identical cache data on APFS and btrfs ([#78](https://github.com/ifdotpy/layr/issues/78)) ([d366b8a](https://github.com/ifdotpy/layr/commit/d366b8aca8d618ce822558dec61e709dc864a384))
* **retest:** line-tables-only debuginfo and a stale artifact sweep ([#80](https://github.com/ifdotpy/layr/issues/80)) ([40e610e](https://github.com/ifdotpy/layr/commit/40e610e0ba2302ad15a983c3c99b308aaf369f76))
* **retest:** make test selection part of the layr CLI ([#82](https://github.com/ifdotpy/layr/issues/82)) ([93e8981](https://github.com/ifdotpy/layr/commit/93e89810b90a382a6c85936757dbd750f7390ba5))

## [0.8.0](https://github.com/ifdotpy/layr/compare/v0.7.0...v0.8.0) (2026-10-31)


### Features

* **du:** Show the disk space of lines and store parts; gc drops pushed automatic states ([#79](https://github.com/ifdotpy/layr/issues/79)) ([bfca40a](https://github.com/ifdotpy/layr/commit/bfca40a519441f9241607c9c305949872f46a5ef))
* **retest:** export a workspace dylib as an llvm-cov object ([#77](https://github.com/ifdotpy/layr/issues/77)) ([bd569c8](https://github.com/ifdotpy/layr/commit/bd569c8124bde0f12edd52bea048b7170a3313dc))

## [0.7.0](https://github.com/ifdotpy/layr/compare/v0.6.1...v0.7.0) (2026-10-31)


### Features

* **retest:** incremental compilation for the instrumented target ([#72](https://github.com/ifdotpy/layr/issues/72)) ([f6a5118](https://github.com/ifdotpy/layr/commit/f6a51182acf056b55d892bc785e5343350983b03))


### Bug Fixes

* **work:** Give written working files the current time ([#76](https://github.com/ifdotpy/layr/issues/76)) ([ca0865f](https://github.com/ifdotpy/layr/commit/ca0865fff04fb83eae1ee1f3b4bf0b626b2e8ff5))

## [0.6.1](https://github.com/ifdotpy/layr/compare/v0.6.0...v0.6.1) (2026-10-31)


### Bug Fixes

* **push:** Require the line names in --include-base ([#73](https://github.com/ifdotpy/layr/issues/73)) ([d15114c](https://github.com/ifdotpy/layr/commit/d15114c6940b6e90e5de49d16b3a8c2ebb462c1c))

## [0.6.0](https://github.com/ifdotpy/layr/compare/v0.5.2...v0.6.0) (2026-10-31)


### Features

* **lines:** Keep other lines' work out of new lines and pushes ([#70](https://github.com/ifdotpy/layr/issues/70)) ([901f850](https://github.com/ifdotpy/layr/commit/901f850d0c5b9e329c7feb0a7f76fff7d1f3cc35))

## [0.5.2](https://github.com/ifdotpy/layr/compare/v0.5.1...v0.5.2) (2026-10-30)


### Performance

* **lock:** Prepare clean, park, pick and undo before the store lock ([#69](https://github.com/ifdotpy/layr/issues/69)) ([720d3f5](https://github.com/ifdotpy/layr/commit/720d3f58c9c9d00b40acf20e9a6505679470d552))
* **merge:** Copy the caches of a merged line without the project lock ([#65](https://github.com/ifdotpy/layr/issues/65)) ([2e156eb](https://github.com/ifdotpy/layr/commit/2e156ebdd18d0eedc53277f41c9c8cbe6e3f82f5))
* **replay:** Merge the replayed states before the store lock ([#66](https://github.com/ifdotpy/layr/issues/66)) ([075c91d](https://github.com/ifdotpy/layr/commit/075c91d22a8e332adebb09f07bc2531f42ba8dac))

## [0.5.1](https://github.com/ifdotpy/layr/compare/v0.5.0...v0.5.1) (2026-10-30)


### Performance

* **push:** Check only new states for secrets before a git push ([#61](https://github.com/ifdotpy/layr/issues/61)) ([3dfec8e](https://github.com/ifdotpy/layr/commit/3dfec8ea81c6d9cd9f3a9b31abe516a2720c36ff))
* **save:** Copy only what changed since the previous save ([#63](https://github.com/ifdotpy/layr/issues/63)) ([515dd5f](https://github.com/ifdotpy/layr/commit/515dd5f26fefd43eba2f5af2d5186760c5221cc4))

## [0.5.0](https://github.com/ifdotpy/layr/compare/v0.4.0...v0.5.0) (2026-10-30)


### ⚠ BREAKING CHANGES

* **cli:** Remove the cache command group

### Features

* **cache:** Give caches to protected lines only through approved merges ([19f6e2e](https://github.com/ifdotpy/layr/commit/19f6e2ef3b0d1deb5f21346232c4017636fbb0b2))
* **cache:** Keep caches as folder overlays that lines inherit and merge ([8f98269](https://github.com/ifdotpy/layr/commit/8f982699f2bd592a6f10ca19346bca127379cf61))
* **cache:** Key caches by platform and lock files; fetch only the own platform ([c9277ad](https://github.com/ifdotpy/layr/commit/c9277ade0041c1f8d6dafbd5015256023effbb46))
* **cache:** Let trusted machines open caches from a store; push past pruned bases ([bc0de81](https://github.com/ifdotpy/layr/commit/bc0de81af9e00628b512f697823c3a2864538d5f))
* **checks:** Run creator-defined checks on isolated laptop runners and macOS ([655e503](https://github.com/ifdotpy/layr/commit/655e5039ff6caf3519b2a53fd8d262ca8b4ad7da))
* **cli:** Remove the cache command group ([ab90bfe](https://github.com/ifdotpy/layr/commit/ab90bfe8bdad6e898d211b1f60d88cf653e6019a))
* **cli:** Show the commit and build date in layr version ([de025da](https://github.com/ifdotpy/layr/commit/de025da2610d70c4c4f435d8a8803291c05cd49f))
* **cli:** Use the build identity in layr version ([6635a0c](https://github.com/ifdotpy/layr/commit/6635a0c8d870711220753bc59524a802927223de))
* **git:** Make the .git folder of a line read only for git ([71e1b93](https://github.com/ifdotpy/layr/commit/71e1b9315a042e10a00a413041af8bc4d16b2710))
* **git:** Refuse layr writes after git writes in a line ([62d0869](https://github.com/ifdotpy/layr/commit/62d08695394ebb23f755de2f9c30da33431dd170))
* **git:** Report progress of long Git imports and keep squash commits out of history ([e8878bb](https://github.com/ifdotpy/layr/commit/e8878bb80e3aedfdc7215cedbf1340fa6cf6455c))
* **merge:** Name the ready merge command in the protected-line refusal ([8f39659](https://github.com/ifdotpy/layr/commit/8f3965916505dfac084a680b762fa4d2daa32df7))
* **overlay:** Add overlay properties: history, quiet, merge union-theirs, gc ([39b3b47](https://github.com/ifdotpy/layr/commit/39b3b4736d6939716b0720cbb930192ab1f3c3ed))
* **remote:** Push each layer to its own remotes ([d9e71bf](https://github.com/ifdotpy/layr/commit/d9e71bf57bb676350fbc0dccdc4f1a759d63ee04))
* **retest:** Add layr-retest: run only the tests that a change can affect ([bf1e1a5](https://github.com/ifdotpy/layr/commit/bf1e1a5128ceb93cd1a84dcc7ef90db6b3e66d05))
* **retest:** export coverage while tests run, full run when a killed bin changes ([#56](https://github.com/ifdotpy/layr/issues/56)) ([671d03b](https://github.com/ifdotpy/layr/commit/671d03b156015206141673fef556edd26a04524e))
* **retest:** Move carried records to the lines of the new state ([5e9cb2e](https://github.com/ifdotpy/layr/commit/5e9cb2e4b2205e761fefbe77f1b101c8d0e3f1d6))
* **retest:** One profile folder per test, children included ([fb8e3f7](https://github.com/ifdotpy/layr/commit/fb8e3f7436776dd570a71f652576aab9332d39f6))
* **retest:** Records in a cache overlay, and dependency rules ([80e21e5](https://github.com/ifdotpy/layr/commit/80e21e5d683477b1b5df39a85871ec844ee24c5b))
* **retest:** Vitest, jest and pytest-testmon adapters ([864074b](https://github.com/ifdotpy/layr/commit/864074b3562dfa5e06f1fb56e966fb63f7375007))
* **runner:** Launch the isolated runner VM from a user LaunchAgent ([ac48b13](https://github.com/ifdotpy/layr/commit/ac48b13d1cbbcddad57c5ef979d99e5c1c8db86c))
* **store:** Guard local store formats and negotiate daemon compatibility ([21e0948](https://github.com/ifdotpy/layr/commit/21e09480ccf90713540f4c6d9e17a429fac35374))


### Bug Fixes

* **apfs:** Confine APFS filesystem operations and isolate operator-only tests ([cdaf1f0](https://github.com/ifdotpy/layr/commit/cdaf1f05f95840f1dd06e3d3fbaf304ad73d952a))
* **bootstrap:** Repair missing published tips and support paged machine bootstrap with legacy fallback ([2ce62a4](https://github.com/ifdotpy/layr/commit/2ce62a440db691ce1d6587345a6bc1f7223ede0f))
* **cache:** Apply cache retention in save --caches and line delete, without the daemon ([33215e6](https://github.com/ifdotpy/layr/commit/33215e6d2c3bfae382f9718769c60670c567837b))
* **cache:** Recover cache replacements only under the project lock ([8af2e40](https://github.com/ifdotpy/layr/commit/8af2e402629bc8796f1d799c8a2cfcd66aa91b9b))
* **cache:** Recover interrupted cache replacements and verify renamed directory identities ([26aae93](https://github.com/ifdotpy/layr/commit/26aae93beb4c2fc268f92da89494e1e255e90588))
* **cache:** Remove read-only cache directories after conversion ([e589938](https://github.com/ifdotpy/layr/commit/e5899383928113288e29c1d90e8ca9a0c3cea685))
* **cache:** Restore read-only cache modes around directory renames ([8d77ae9](https://github.com/ifdotpy/layr/commit/8d77ae91ab968312927f435e3bb0f36c858b9ea8))
* **cli:** Expose the existing help text to the early command check ([edd8792](https://github.com/ifdotpy/layr/commit/edd87926d7850b45357095d0dd206f0a7cefd6a7))
* **cli:** Handle command help and unknown commands before store access ([a5312a6](https://github.com/ifdotpy/layr/commit/a5312a6f498e1d0dd0993ef541c0bb3a9dde8359))
* **daemon:** Keep request failure context when daemon negotiation fails ([e129124](https://github.com/ifdotpy/layr/commit/e129124cc27fcdfc0528e2aceb97a35511fa5fc5))
* **gateway:** Open the machine slot before reading the paper slot through a gateway ([60c091a](https://github.com/ifdotpy/layr/commit/60c091ab6d49c528174451d2bcbf43926c5272c4))
* **git:** Detect damaged Git caches and rebuild exact history from durable records ([f56d9c1](https://github.com/ifdotpy/layr/commit/f56d9c140b81ed7f95430fdda04a18b7cf6b1043))
* **git:** Keep replacing a non-regular git HEAD in the line ([e779dba](https://github.com/ifdotpy/layr/commit/e779dbae97d13d9db2ba45d78be7fd0a20cb8023))
* **git:** Preserve standalone caller Git settings when applying patches ([35c5386](https://github.com/ifdotpy/layr/commit/35c5386aaa3a87607bb4b0474c2c84a50a509eb9))
* **git:** Sign export commits as git commit does ([1c63e3c](https://github.com/ifdotpy/layr/commit/1c63e3c65da097cfb3484be9e5130474c19f3de8))
* **git:** Skip the git HEAD guard when the head has no git commit ([ce4c496](https://github.com/ifdotpy/layr/commit/ce4c496189762b2f4ed4a406c124437779988bd9))
* **grep:** Preserve the grep short option that omits filenames ([28e535e](https://github.com/ifdotpy/layr/commit/28e535edffc07ce3bf834814906359710996634e))
* **identity:** Publish one machine identity under concurrent initialization ([e879f91](https://github.com/ifdotpy/layr/commit/e879f91408b87b7729ca627e86fc44ec9ff4eb1f))
* **lock:** Build the status preparation only on macOS ([af2cace](https://github.com/ifdotpy/layr/commit/af2cace0c0794b4546500fe6ff8344221bf3c000))
* **ls-files:** Explain a revision argument instead of printing nothing ([15e0a08](https://github.com/ifdotpy/layr/commit/15e0a0813c69fa1895f86969b3873130d73f9fc3))
* **merge:** Refuse a merge only over open conflict markers ([0ba22cc](https://github.com/ifdotpy/layr/commit/0ba22cca3ed1c471abb803c21d00668448acafbf))
* **merge:** Refuse merges over open conflicts and saves of conflict markers ([49daf3f](https://github.com/ifdotpy/layr/commit/49daf3f6f28d3157bf1114c1b3344a814f0966c0))
* Port Studio regression fixes to layr 0.4 ([b823f6a](https://github.com/ifdotpy/layr/commit/b823f6a5c2495e222bd7a5d13ad280d8bcd8a11c))
* **push:** Checkpoint and remove WAL sidecars without replaying stale journals ([379741e](https://github.com/ifdotpy/layr/commit/379741ee0f2e6a58819a9baec17b3d29b8ece926))
* **push:** Keep pending parts age-bound until a referencing tree publishes ([fbd8711](https://github.com/ifdotpy/layr/commit/fbd87117c0bbbfc634a3d42feda7d9f5f5f0eace))
* **push:** Persist active push hashes and authenticate pending parts on recovery ([b9abdcd](https://github.com/ifdotpy/layr/commit/b9abdcd02ac890ae0ddac0af11672ac2505f1f10))
* **push:** Preserve authenticated tree depth on cache misses and scale cache with payload ([c8f8a03](https://github.com/ifdotpy/layr/commit/c8f8a0350bfb0760b6c944f8ceba2092c936819f))
* **push:** Rebuild recovered push caches from authenticated remote trees ([13702ca](https://github.com/ifdotpy/layr/commit/13702cac3f2ce102077a99c4e8709b65c43ecfc0))
* **push:** Recover pushed tree cache depth after a lost index checkpoint ([9e6da39](https://github.com/ifdotpy/layr/commit/9e6da396d79010a5d7b6cfe827f50ee8aa922808))
* **push:** Track newly published attempts for parent hydration after cache eviction ([27a59c3](https://github.com/ifdotpy/layr/commit/27a59c34845fd098a12389000cdcdb9220807c8c))
* **remote:** Ignore vanished internal publication files during remote listing ([b6730cc](https://github.com/ifdotpy/layr/commit/b6730ccc97aad70e184efd14f069a7040e85dd7d))
* **runner:** Keep filesystem checks within prepared volumes and preserve fixture traversal ([9481ee5](https://github.com/ifdotpy/layr/commit/9481ee514b26e10f7ef10057339e933ca78b3871))
* **runner:** Keep Git configuration private inside automatic Mac checks ([2491ff5](https://github.com/ifdotpy/layr/commit/2491ff5fc39a1695cc7650f62e930cc0b6e4eacb))
* **runner:** Pace runner sync and keep transient grants outside daemon runtime ([22a3de7](https://github.com/ifdotpy/layr/commit/22a3de7b01a0d336c8c70043b53784d3d94d6c23))
* **runner:** Pass a private gateway token file path to Mac runner agents ([7761447](https://github.com/ifdotpy/layr/commit/7761447bfa93a14dfbc3a0599ce75476e5a67811))
* **runner:** Use prepared Git and an absolute test user switch command ([93c86e9](https://github.com/ifdotpy/layr/commit/93c86e93ec840e6891b9c415911a964caa41f314))
* **save:** Compare confirmed file changes when deciding a new auto state ([85be4e0](https://github.com/ifdotpy/layr/commit/85be4e017c6e3c89459acdd1442c78c69f570ed7))
* **save:** Make an unchanged save name the existing state ([58cce58](https://github.com/ifdotpy/layr/commit/58cce58d0b70ae5b21562fede19916b57fb052ee))
* **save:** Treat unchanged empty files as unchanged in a save ([1766b66](https://github.com/ifdotpy/layr/commit/1766b662fa5b6bb6b618b8fe29bcf7e413548c55))
* **status:** Stop status from holding the project lock and spending minutes in radar ([2b2b56e](https://github.com/ifdotpy/layr/commit/2b2b56e143c21d870dae7f0c098789abfa88cf8f))
* **store:** Create store paper keys only on a confirmed private terminal ([d45fcfc](https://github.com/ifdotpy/layr/commit/d45fcfc59b80aca389a3e668f13e78e993151b32))
* **store:** Preserve full imported Git history across encrypted stores ([fca30ff](https://github.com/ifdotpy/layr/commit/fca30ff92f781be9ab15e1594c2decdcf7031097))
* **store:** Resolve the store root once so links above it work ([98422a6](https://github.com/ifdotpy/layr/commit/98422a64b8e3020302f8f2ffebce3a772b0a1c6a))
* **store:** Wait for a concurrent initial store marker before legacy checks ([464c414](https://github.com/ifdotpy/layr/commit/464c414b7609cbf9b0ea784da92d907152bd5db8))


### Performance

* **apfs:** Clone a tree with nested layers at once on APFS ([e9bb63f](https://github.com/ifdotpy/layr/commit/e9bb63f5d16ac2dcdea13f18576ccbad1ed7981f))
* **gateway:** Page large gateway listings and limit incremental pushes to new state prefixes ([1740dcf](https://github.com/ifdotpy/layr/commit/1740dcf9f38ed18b636ac6e4382ad8fa747fb479))
* **git:** Avoid per-object filesystem flushes when rebuilding Git cache ([3b215cf](https://github.com/ifdotpy/layr/commit/3b215cfda1e0e72ab5d953d6ded389c82b664842))
* **git:** Build imported states from the parent state and the commit's changes ([2b0b38c](https://github.com/ifdotpy/layr/commit/2b0b38cc5685b32c258f530d45d7c4a9a600ad78))
* **git:** Hash only Git objects read by a bridge operation ([d1df6fb](https://github.com/ifdotpy/layr/commit/d1df6fb88b78f90e6a15ebcf53e524b110885fd7))
* **git:** Verify only ancestor objects for Git path queries and record validation evidence ([8bade8d](https://github.com/ifdotpy/layr/commit/8bade8d23903513284f857174192da55e2554e6a))
* **lock:** Copy caches before the store lock in save --caches on APFS ([50d9668](https://github.com/ifdotpy/layr/commit/50d9668c1f3a25762d8cb5e47498e837e8283023))
* **lock:** Copy only the named paths for a restore of the working folder ([6c833ec](https://github.com/ifdotpy/layr/commit/6c833ecdf4098bd440827ab512e2d4f1d59b21fd))
* **lock:** Delete trees after the last store lock; log slow lock holds ([dd29bf4](https://github.com/ifdotpy/layr/commit/dd29bf49510b2d82ef6ebab362a58eb55a611e06))
* **lock:** Merge and reset --hard before the store lock ([4407320](https://github.com/ifdotpy/layr/commit/440732059f86156391cc4dd0d50809d8eff6c7f1))
* **lock:** Move the remaining slow deletions of gc and undo out of the store lock ([6000c8d](https://github.com/ifdotpy/layr/commit/6000c8dbb7e91ba5a81066c265e18cf8932a90c9))
* **lock:** Push to git remotes without the store lock ([8e03b1c](https://github.com/ifdotpy/layr/commit/8e03b1ca9f411d401db7fd91154e5e2347f1a07c))
* **lock:** Read the working folder for add before the store lock ([6d85fd1](https://github.com/ifdotpy/layr/commit/6d85fd1eb771643ea58adfef65d4df227676ff26))
* **lock:** Stage the files of save -a with the read of add, before the lock ([8eb0c5a](https://github.com/ifdotpy/layr/commit/8eb0c5a95137da6631beec81b239b8dacb629a44))
* **lock:** Write the automatic state before the store lock; base the after state on it ([62fec4b](https://github.com/ifdotpy/layr/commit/62fec4b28c1a018ff9cba6163128ec2340e2e7ae))
* **push:** Bound local push cache with compressed content-addressed subtrees ([08d081d](https://github.com/ifdotpy/layr/commit/08d081d68f11f86c0c84cb0774d7929b25536139))
* **push:** Group push progress in a recoverable WAL without per-state fsyncs ([3f768e3](https://github.com/ifdotpy/layr/commit/3f768e3ed8d19a9cf5fe1bc9c886025cf697c954))
* **push:** Keep secret checks and parent hydration bounded for incremental store pushes ([73ad990](https://github.com/ifdotpy/layr/commit/73ad990f8b77a6206179a3644aab8533d5898b7d))
* **retest:** Exit before the build when nothing can change, cheaper status, opt-in cache save ([#53](https://github.com/ifdotpy/layr/issues/53)) ([1abb0b7](https://github.com/ifdotpy/layr/commit/1abb0b7f9cfa6f53b404a451b9ee4ca7f90d9e52))
* **retest:** Faster coverage export ([#50](https://github.com/ifdotpy/layr/issues/50)) ([c5acf77](https://github.com/ifdotpy/layr/commit/c5acf773319870cb89882e3f15db5cf7e175e70a))
* **runner:** Read the prepared Mac Cargo registry without per-run copies ([d1ff0ce](https://github.com/ifdotpy/layr/commit/d1ff0ce093547e879f93c7fb178d5f265ef5428b))
* **scan:** Keep content scans fast with hard links and leave ignored files out of the radar ([1edb179](https://github.com/ifdotpy/layr/commit/1edb1791952885c3a6ec68b60de621f9f983ffc2))
* **store:** Reuse parent trees and encode store metadata without generic JSON copies ([bdf6529](https://github.com/ifdotpy/layr/commit/bdf65290110a50bdfdecda0ef2d7a3d7fa764324))
* **store:** Scale store transfers with bounded caches and parent snapshots ([729da5d](https://github.com/ifdotpy/layr/commit/729da5d92b0ca5ee97e6a1a06b5f0d4eb81661b8))
* **store:** Use exact typed canonical tree encoding on store reads and writes ([ce45204](https://github.com/ifdotpy/layr/commit/ce452040dc37704cd22dd0ed0ea770d2e7cb8ece))
