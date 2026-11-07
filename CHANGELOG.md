# Changelog

## [0.39.1](https://github.com/ifdotpy/layr/compare/v0.39.0...v0.39.1) (2026-11-07)


### Bug Fixes

* **caches:** a command stopped at its limit takes its child processes with it; the warm limit is 2 h ([#416](https://github.com/ifdotpy/layr/issues/416)) ([6d01b59](https://github.com/ifdotpy/layr/commit/6d01b592b97e15a34c6c00ab38b97fe6499ac1ed))

## [0.39.0](https://github.com/ifdotpy/layr/compare/v0.38.1...v0.39.0) (2026-11-07)


### Features

* **remote:** Private and Public store modes keep plain objects without keys ([#395](https://github.com/ifdotpy/layr/issues/395)) ([32580f1](https://github.com/ifdotpy/layr/commit/32580f101ebd62831bcf2fc9b6a8366cb3e5f730))


### Bug Fixes

* **caches:** the copy of a node prune lives outside the work tree; store-version names the layr version ([#411](https://github.com/ifdotpy/layr/issues/411)) ([eb31df9](https://github.com/ifdotpy/layr/commit/eb31df94c552aa34eb1f5fe6837799f2a77846fd))
* **caches:** a union of two caches takes each cargo unit whole from one side ([#408](https://github.com/ifdotpy/layr/issues/408)) ([106b071](https://github.com/ifdotpy/layr/commit/106b071a9b9d6472fc829add95f5683242803d77))
* **caches:** a node prune, warm or recovery removes node_modules.layr-broken left by an interrupted restore ([#407](https://github.com/ifdotpy/layr/issues/407)) ([a677418](https://github.com/ifdotpy/layr/commit/a6774185ad57dad4a1b7c362b0aa8f9f39c6fee4))
* **checks:** a line that checks.lines names after its delete is skipped, not fatal ([#412](https://github.com/ifdotpy/layr/issues/412)) ([e36eb06](https://github.com/ifdotpy/layr/commit/e36eb066820cfbdce7d15ef3f6aabdcd243375b8))
* **remote:** hash every visited file on push instead of trusting size and mtime ([f6b0c0c](https://github.com/ifdotpy/layr/commit/f6b0c0c67e7529b382db506fa69faf22f3d31d24))
* **remote:** end the heads index publish loop through its condition ([0d03dc8](https://github.com/ifdotpy/layr/commit/0d03dc8674e3a7df129ae6267280cc7b182ca7bf))


### Performance

* **cli:** one epilogue database connection and fewer request allocations ([44b1d42](https://github.com/ifdotpy/layr/commit/44b1d42ee9e9fbd9f46db8010e6539979bbc7f11))
* **core:** one allocation per output line and a schema check query built once ([35e8c1a](https://github.com/ifdotpy/layr/commit/35e8c1a9195f0e9c9ea280246b29282bd7320ce6))
* **diff:** batch pin bookkeeping and release diff pins on the open database ([6b5fa95](https://github.com/ifdotpy/layr/commit/6b5fa953f63d6c3cda57c4da1f8c2133edfa1a39))
* **free:** confirm live pin holders with one batched process lookup ([8437920](https://github.com/ifdotpy/layr/commit/8437920ec4adb781668c740e53bfd99d1076e476))
* **history:** select imported changes by path before loading blobs ([69cbedf](https://github.com/ifdotpy/layr/commit/69cbedfc21a026f1e6265a38fc80ec89fd75312f))
* **history:** linear grep, borrowed diff rendering, single-load history walk and shared edit operations ([66234a5](https://github.com/ifdotpy/layr/commit/66234a56f79f3736aaefe8959f3a40fe6e6eb0c6))
* **line:** record a line batch in one transaction and resolve the author once ([b25a660](https://github.com/ifdotpy/layr/commit/b25a660838475eec0da515ce5304b37a5811aff2))
* **merge,git:** compute merge sides once, prepare git export once, stream loose objects ([c4b4311](https://github.com/ifdotpy/layr/commit/c4b431111472ee01de70bcdefc67d5d81c103956))
* **overlay:** open each overlay project once per read helper ([e261cf2](https://github.com/ifdotpy/layr/commit/e261cf29a9aaa65e8d0e72dacbadd314c113bfe3))
* **overlay:** open each overlay project once per listing ([a0ae614](https://github.com/ifdotpy/layr/commit/a0ae614223866465a1b9d6379dc5b0e24fb56d9d))
* **radar:** write rebuildable overlap caches without fsync ([85cb425](https://github.com/ifdotpy/layr/commit/85cb4259e39731c4234a852a57efe2a2ae5bd082))
* **radar:** skip overlap compares when the base is the head or the states match ([05760b1](https://github.com/ifdotpy/layr/commit/05760b1407415601d242d5191da760408c830f55))

## [0.38.1](https://github.com/ifdotpy/layr/compare/v0.38.0...v0.38.1) (2026-11-06)


### Bug Fixes

* **remote:** renew the gateway read proof before expiry and on 401 ([#403](https://github.com/ifdotpy/layr/issues/403)) ([01f4e56](https://github.com/ifdotpy/layr/commit/01f4e5658fb2d3663466fd4c279a567a05c22330))

## [0.38.0](https://github.com/ifdotpy/layr/compare/v0.37.0...v0.38.0) (2026-11-06)


### ⚠ BREAKING CHANGES

* cut compatibility shims ([#372](https://github.com/ifdotpy/layr/issues/372))
* **caches:** caches live in cache overlays only; old cache snapshots of states go ([#389](https://github.com/ifdotpy/layr/issues/389))
* **merge:** remove the options that did nothing; pick --continue says what to do ([#380](https://github.com/ifdotpy/layr/issues/380))
* **park:** park keeps the changes as a line without a folder ([#375](https://github.com/ifdotpy/layr/issues/375))
* **remote:** push roles: remote.&lt;name&gt;.push required|optional|off ([#374](https://github.com/ifdotpy/layr/issues/374))

### Features

* **api:** Add typed store-version command contract ([#364](https://github.com/ifdotpy/layr/issues/364)) ([fe65fbc](https://github.com/ifdotpy/layr/commit/fe65fbc6e7ef450fd2d55088563fe7a3e2e65fa0))
* **caches:** caches live in cache overlays only; old cache snapshots of states go ([#389](https://github.com/ifdotpy/layr/issues/389)) ([45d9039](https://github.com/ifdotpy/layr/commit/45d9039326926a038b1554e6402790b3e39cd52a))
* **daemon:** store-version reports the daemon build; follow warns on another build ([#377](https://github.com/ifdotpy/layr/issues/377)) ([41834d9](https://github.com/ifdotpy/layr/commit/41834d9e0e901c40bbb6af9de6d4903585072834))
* **park:** park keeps the changes as a line without a folder ([#375](https://github.com/ifdotpy/layr/issues/375)) ([ea5514d](https://github.com/ifdotpy/layr/commit/ea5514de360d3cf138b9fe2a7ce295e374b8ff83))
* **queue:** a queue entry with a conventional title lands as one state with that title ([#397](https://github.com/ifdotpy/layr/issues/397)) ([1187fb5](https://github.com/ifdotpy/layr/commit/1187fb5d46a0b91879439b1040fbcff2152d095a))
* **release:** layr release prepare writes the next version and its changelog as release-please did ([#394](https://github.com/ifdotpy/layr/issues/394)) ([6b03a0c](https://github.com/ifdotpy/layr/commit/6b03a0cff717dcfaf57d650b9c2f1578ebc5e325))
* **remote:** push roles: remote.&lt;name&gt;.push required|optional|off ([#374](https://github.com/ifdotpy/layr/issues/374)) ([3240b75](https://github.com/ifdotpy/layr/commit/3240b759fa188d63b32ba664424824f70bd9f960))
* **usage:** count the commands that run on this machine ([#376](https://github.com/ifdotpy/layr/issues/376)) ([fb1b888](https://github.com/ifdotpy/layr/commit/fb1b888c0dec38981da8c225452224bda2e679ca))


### Bug Fixes

* **attest:** root revokes the attestations of every user of its machine ([#386](https://github.com/ifdotpy/layr/issues/386)) ([e0fa6fd](https://github.com/ifdotpy/layr/commit/e0fa6fdbdf8c39d6fd69b833194fe6cfdaccf9b8))
* **caches:** a stopped node prune never puts a partial copy back; a damaged folder is repaired before its mark goes ([#391](https://github.com/ifdotpy/layr/issues/391)) ([1234fde](https://github.com/ifdotpy/layr/commit/1234fdeeb64bf354cdee687cfd8169a092169002))
* **caches:** a cargo warm build runs every sweep command when one fails ([#388](https://github.com/ifdotpy/layr/issues/388)) ([5cffd30](https://github.com/ifdotpy/layr/commit/5cffd307f176ca5dfb3723601bb10259b91ef701))
* **caches:** copy node_modules with the system cp on macOS ([#383](https://github.com/ifdotpy/layr/issues/383)) ([d270f98](https://github.com/ifdotpy/layr/commit/d270f98c9eb581f0afd028c2087056d122a7508e))
* **checks:** a check job that waits on this machine says why ([#385](https://github.com/ifdotpy/layr/issues/385)) ([7636e93](https://github.com/ifdotpy/layr/commit/7636e9314b3356494e4f5b36324a7b656533949d))
* **checks:** checks.changes plans the newest version of each open change, newest states first ([#371](https://github.com/ifdotpy/layr/issues/371)) ([eb79dcf](https://github.com/ifdotpy/layr/commit/eb79dcfa78cdcb686da68d2427d69b99f6ddc3ee))
* **checks:** a save-back records the source times of the check copy ([#379](https://github.com/ifdotpy/layr/issues/379)) ([f45359e](https://github.com/ifdotpy/layr/commit/f45359eef502ac6cccdf9a433908a276fd52e623))
* **db:** a temporary SQLite lock (SQLITE_PROTOCOL, SQLITE_BUSY) is retried instead of failing the command ([#401](https://github.com/ifdotpy/layr/issues/401)) ([8b14acc](https://github.com/ifdotpy/layr/commit/8b14accf742b9bb8c0305b499c506af324f97bbc))
* **gc:** reserve a tree before eviction removes it, so a reader cannot pin it meanwhile ([#378](https://github.com/ifdotpy/layr/issues/378)) ([f418d0c](https://github.com/ifdotpy/layr/commit/f418d0c6191122792057c4d16aad3560c337ee9b))
* **merge:** remove the options that did nothing; pick --continue says what to do ([#380](https://github.com/ifdotpy/layr/issues/380)) ([59ecc34](https://github.com/ifdotpy/layr/commit/59ecc34ac9e52a32798444ceca950f406b8d32ac))
* **remote:** a store push leaves out a state that a free step queued ([#400](https://github.com/ifdotpy/layr/issues/400)) ([a027735](https://github.com/ifdotpy/layr/commit/a027735531a3bcbb190ef553b88b98e5106f8e1d))
* **remote:** a cache sweep leaves a remote tree that cannot be decoded; main builds again ([#399](https://github.com/ifdotpy/layr/issues/399)) ([7ce9845](https://github.com/ifdotpy/layr/commit/7ce9845cf5be34a750f0391f386abb48850c91df))
* **remote:** a push takes a remote base tree that cannot be read as absent ([#398](https://github.com/ifdotpy/layr/issues/398)) ([be9917a](https://github.com/ifdotpy/layr/commit/be9917a5007db41b12f2d6c242cf7c5907540cdb))
* **remote:** a cache overlay push holds the overlay's project lock only for its key records ([#392](https://github.com/ifdotpy/layr/issues/392)) ([3c39de3](https://github.com/ifdotpy/layr/commit/3c39de388417827df1c3aace244610c91fbaebb9))
* **service:** the macOS user daemon applies person requests and runs the queue on its tick ([#373](https://github.com/ifdotpy/layr/issues/373)) ([d3a7650](https://github.com/ifdotpy/layr/commit/d3a7650c78c56b1983d265e019cd7a3952ae5f28))
* **store:** the root snapshot check covers the volumes of the store ([#381](https://github.com/ifdotpy/layr/issues/381)) ([c10f796](https://github.com/ifdotpy/layr/commit/c10f796d4ac7bddcf4adbf2ad114ead2bdc8c71a))


### Performance

* **oplog:** Limit indexed records before decoding ([#241](https://github.com/ifdotpy/layr/issues/241)) ([adb4a7c](https://github.com/ifdotpy/layr/commit/adb4a7c2c6110be40102ce3a415ed61c67482f77))
* **remote:** a store push checks its tips 32 at a time ([#387](https://github.com/ifdotpy/layr/issues/387)) ([bc565a5](https://github.com/ifdotpy/layr/commit/bc565a59e1f78dfd074b9a01c13e2501c2a99fe5))


### Code Refactoring

* cut compatibility shims ([#372](https://github.com/ifdotpy/layr/issues/372)) ([2f88053](https://github.com/ifdotpy/layr/commit/2f8805376695d19c965db7aa7e43d72abe528f0f))

## [0.37.0](https://github.com/ifdotpy/layr/compare/v0.36.0...v0.37.0) (2026-11-06)


### ⚠ BREAKING CHANGES

* **remote:** remove the Desk v1 import ([#357](https://github.com/ifdotpy/layr/issues/357))

### Features

* **review:** path owners and approvals that a change of some paths keeps ([#353](https://github.com/ifdotpy/layr/issues/353)) ([4b04e33](https://github.com/ifdotpy/layr/commit/4b04e33eb68ce45c0bf75a17503e73676ffbc320))
* **status:** Add overlap and ignored output opt-outs ([#366](https://github.com/ifdotpy/layr/issues/366)) ([1c411d9](https://github.com/ifdotpy/layr/commit/1c411d9b1486fcb4ddf30a3bdf9287cc2d86e1d8))


### Bug Fixes

* **caches:** a node cache prune or warm step leaves package-lock.json and package.json of the line as they were ([#356](https://github.com/ifdotpy/layr/issues/356)) ([652ed0a](https://github.com/ifdotpy/layr/commit/652ed0a4224e2af6403a86f7fffe31a8ee45e67d))
* **caches:** relocate also reads the binary dep-info of a unit ([#347](https://github.com/ifdotpy/layr/issues/347)) ([4e464b8](https://github.com/ifdotpy/layr/commit/4e464b8f53b44bcd96fd61fbc1577c33a8dcea73))
* **deps:** update zstd-safe for RUSTSEC-2026-0398 and RUSTSEC-2026-0403 ([#367](https://github.com/ifdotpy/layr/issues/367)) ([2cb1656](https://github.com/ifdotpy/layr/commit/2cb16561416191846fc8fe3c4c3efe4764f94fda))
* **gateway:** a release file upload goes again while the gateway restarts ([#351](https://github.com/ifdotpy/layr/issues/351)) ([39b58f6](https://github.com/ifdotpy/layr/commit/39b58f634668b9a0fbc563c5b9c10f0e56de8a74))
* **privs:** a command of the caller runs with the caller's PATH, and init --from-git names the git error ([#354](https://github.com/ifdotpy/layr/issues/354)) ([e72c6aa](https://github.com/ifdotpy/layr/commit/e72c6aaa87898ceeac01b87d1ae68916c1c580cd))
* **release:** release attach uploads its public copies without the project lock ([#361](https://github.com/ifdotpy/layr/issues/361)) ([5a1bd31](https://github.com/ifdotpy/layr/commit/5a1bd31ce601e54ce92f9e3f29b5522614066afd))
* **remote:** a chunk pack whose data is gone counts as absent, and the sweep deletes its index ([#360](https://github.com/ifdotpy/layr/issues/360)) ([eaa67f6](https://github.com/ifdotpy/layr/commit/eaa67f655a8f171f6b3a936694d578b183684dc8))
* **remote:** index write takes no project lock, writes the heads first and goes on past an unreadable tree ([#359](https://github.com/ifdotpy/layr/issues/359)) ([f966715](https://github.com/ifdotpy/layr/commit/f9667151a34d8871949008c22787c83a032411b5))
* **service:** honor client tracing and decode requests once ([638d5ac](https://github.com/ifdotpy/layr/commit/638d5acc55c24ea6fcfc3cc4e7f3d23f915b185c))
* **service:** Honor client tracing and decode requests once ([#370](https://github.com/ifdotpy/layr/issues/370)) ([638d5ac](https://github.com/ifdotpy/layr/commit/638d5acc55c24ea6fcfc3cc4e7f3d23f915b185c))
* **trust:** check the whole record of a limited machine and drop trusts of untrusted signers ([#352](https://github.com/ifdotpy/layr/issues/352)) ([b182e74](https://github.com/ifdotpy/layr/commit/b182e74cfc99d9bd949926fb31fab94b9a763b52))


### Performance

* **apfs:** line copies clone hard-linked files one by one instead of copying the whole tree ([#355](https://github.com/ifdotpy/layr/issues/355)) ([bd26fb1](https://github.com/ifdotpy/layr/commit/bd26fb1f59fff4c324a2581b148ae054bc01ed56))
* **overlay:** Measure cache sizes only when requested ([#365](https://github.com/ifdotpy/layr/issues/365)) ([e3c4064](https://github.com/ifdotpy/layr/commit/e3c4064812fce07c7891f4d07618d4a150e6d04f))


### Code Refactoring

* **remote:** remove the Desk v1 import ([#357](https://github.com/ifdotpy/layr/issues/357)) ([a33df93](https://github.com/ifdotpy/layr/commit/a33df93b5c8610f262d75be3fe5c8a9cc07f6a8d))

## [0.36.0](https://github.com/ifdotpy/layr/compare/v0.35.0...v0.36.0) (2026-11-06)


### Features

* **probes:** a structure snapshot diff matches lines without their indent ([#350](https://github.com/ifdotpy/layr/issues/350)) ([9e9261f](https://github.com/ifdotpy/layr/commit/9e9261fe415f6219d74eea46f844f8ce3bb21f5e))
* **review:** change diff --since-review --person &lt;login&gt; starts at that person's approval ([#342](https://github.com/ifdotpy/layr/issues/342)) ([5c2670d](https://github.com/ifdotpy/layr/commit/5c2670db0da16920f94a0e59792721b9bc67e310))


### Bug Fixes

* **gateway:** a delete error names its key and the gateway's answer; DELETE is retried like other idempotent requests ([#345](https://github.com/ifdotpy/layr/issues/345)) ([dfdd0a8](https://github.com/ifdotpy/layr/commit/dfdd0a8cc6b6b9fb32e10ebb5348a7ae47a0a247))


### Performance

* **model:** merge_base is linear; trusted_writers_checked skips a second signature check ([#344](https://github.com/ifdotpy/layr/issues/344)) ([eed71bb](https://github.com/ifdotpy/layr/commit/eed71bbc5bd32bff3e1ab2a77a15ac9785b14de3))

## [0.35.0](https://github.com/ifdotpy/layr/compare/v0.34.0...v0.35.0) (2026-11-06)


### Features

* **caches:** a cargo unit whose old sources the new line has moves with it ([#340](https://github.com/ifdotpy/layr/issues/340)) ([6c4d042](https://github.com/ifdotpy/layr/commit/6c4d04260c35701462f42f5fe07909102346a50e))
* **caches:** save --caches --save-back &lt;line&gt; gives a CI job's caches back ([#336](https://github.com/ifdotpy/layr/issues/336)) ([c534759](https://github.com/ifdotpy/layr/commit/c534759cfa15214fddcfa4d982fb5deac837812f))
* **remote:** a broker machine trusts CI jobs inside a delegation ([#338](https://github.com/ifdotpy/layr/issues/338)) ([94e3d9c](https://github.com/ifdotpy/layr/commit/94e3d9c7d53089399aa6c593a858f74c85864847))
* **remote:** limited machine trust for CI jobs: an end, a role, the overlays, and untrust ([#335](https://github.com/ifdotpy/layr/issues/335)) ([e2c4fd8](https://github.com/ifdotpy/layr/commit/e2c4fd8f4a8bfa1e577dbcb45eb4285f2ea187cb))


### Bug Fixes

* **caches:** the cargo prune keeps the build unit of a crate with a cdylib ([#337](https://github.com/ifdotpy/layr/issues/337)) ([786f6ff](https://github.com/ifdotpy/layr/commit/786f6ff8ecf10be009ddae354dc7cfbbef04e8c4))
* **remote:** a push whose base tree is gone uploads a complete tree; a line without a folder can be deleted ([#343](https://github.com/ifdotpy/layr/issues/343)) ([fed3bc7](https://github.com/ifdotpy/layr/commit/fed3bc71f7dc8cd6ffb9414fd74afe1458cdeae6))
* **remote:** the first fetch of an overlay layer writes its seen-records file ([#339](https://github.com/ifdotpy/layr/issues/339)) ([5662c9b](https://github.com/ifdotpy/layr/commit/5662c9be7106b87a2b8c5dbd6dd24fc96be440bf))

## [0.34.0](https://github.com/ifdotpy/layr/compare/v0.33.0...v0.34.0) (2026-11-06)


### Features

* **caches:** a sweep entry with its own target folder, command and env ([#332](https://github.com/ifdotpy/layr/issues/332)) ([fd87e6f](https://github.com/ifdotpy/layr/commit/fd87e6f5f8683eb4bc753610622404e688a69720))
* **change:** change diff --since-review starts at the version you approved ([#326](https://github.com/ifdotpy/layr/issues/326)) ([7d357ab](https://github.com/ifdotpy/layr/commit/7d357ab768943cc4a1228c9ae51b6e6e37019bc4))
* **merge:** a protected merge needs a passing check on each platform of checks.platforms ([#325](https://github.com/ifdotpy/layr/issues/325)) ([c9b85e7](https://github.com/ifdotpy/layr/commit/c9b85e7f38e618e0b30935037aa9cab3bfa2271f))
* **probes:** HTML structure snapshots drop generated ids, times and ignored attributes ([#327](https://github.com/ifdotpy/layr/issues/327)) ([217ff18](https://github.com/ifdotpy/layr/commit/217ff18e8b83ef098d01cc798a85233e81f6c752))


### Bug Fixes

* **caches:** gc=lru removes whole build units and the folders it empties ([#330](https://github.com/ifdotpy/layr/issues/330)) ([9d001fb](https://github.com/ifdotpy/layr/commit/9d001fba9748c78c768b6ebf4665605e8aca59aa))
* **gateway:** reads and create-only writes are asked again while the gateway restarts ([#329](https://github.com/ifdotpy/layr/issues/329)) ([3cd92e8](https://github.com/ifdotpy/layr/commit/3cd92e8014fb66221f5d521112b718bfe1c1713f))
* **paths:** one argument with many lines names the cause ([#328](https://github.com/ifdotpy/layr/issues/328)) ([aa94d71](https://github.com/ifdotpy/layr/commit/aa94d712bd848ed1f6a72078cec8284e4645036e))
* **release:** public release files reach the gateway at the root of the store ([#324](https://github.com/ifdotpy/layr/issues/324)) ([2162af3](https://github.com/ifdotpy/layr/commit/2162af3b4bc13a3c525c6f8a3452f85bf68643aa))
* **remote:** a 404 that says no such project is an error, not a missing object ([#334](https://github.com/ifdotpy/layr/issues/334)) ([7231496](https://github.com/ifdotpy/layr/commit/72314966d04c2dbefb64706bc4b836b9cf83423c))
* **revs:** a Git commit id with several states resolves to one of them ([#333](https://github.com/ifdotpy/layr/issues/333)) ([7dc73e5](https://github.com/ifdotpy/layr/commit/7dc73e5d90cdaf8279a6c54e70cd8d3fc36597e0))

## [0.33.0](https://github.com/ifdotpy/layr/compare/v0.32.0...v0.33.0) (2026-11-06)


### Features

* **checks:** checks.after runs a check only after the checks it needs passed ([#316](https://github.com/ifdotpy/layr/issues/316)) ([2781b1b](https://github.com/ifdotpy/layr/commit/2781b1b689b8993ea94d350b46b271deb48a3f35))


### Bug Fixes

* **remote:** a Git pack that lists commits it does not hold does not stop a clone ([#321](https://github.com/ifdotpy/layr/issues/321)) ([9d29fd2](https://github.com/ifdotpy/layr/commit/9d29fd20d664f7c72dbc763092a43dd73ebb3bef))


### Performance

* **remote:** bound transfer memory and zstd decoding, size the retest gate to the host ([#319](https://github.com/ifdotpy/layr/issues/319)) ([3febac2](https://github.com/ifdotpy/layr/commit/3febac28c7338c27c6e386b7d00794574cf76692))

## [0.32.0](https://github.com/ifdotpy/layr/compare/v0.31.0...v0.32.0) (2026-11-06)


### Features

* **daemon:** a daily garbage report instead of the hourly repair ([#315](https://github.com/ifdotpy/layr/issues/315)) ([d117748](https://github.com/ifdotpy/layr/commit/d11774811a55d82d5e1c41e89a6607b8897aa8b4))
* **ownership:** free data in the operation that makes it dead ([#304](https://github.com/ifdotpy/layr/issues/304)) ([3ed4f30](https://github.com/ifdotpy/layr/commit/3ed4f301223f95fd1c6d8fc5b94bef4920fc0026))
* **queue:** push merged lines and apply person requests in the daemon ([#311](https://github.com/ifdotpy/layr/issues/311)) ([740b2fa](https://github.com/ifdotpy/layr/commit/740b2fad53746a6918935864f0438e042a010353))
* **release:** attach --public-files writes plaintext release files for the gateway ([#314](https://github.com/ifdotpy/layr/issues/314)) ([c12637c](https://github.com/ifdotpy/layr/commit/c12637cc5eb8a946810074ff91d28ffc6d2604fe))


### Bug Fixes

* **cache:** a process that only reads a cache folder does not stop its save ([#306](https://github.com/ifdotpy/layr/issues/306)) ([0de0f1d](https://github.com/ifdotpy/layr/commit/0de0f1de581edcfc589f1f5333c3d054c4f0c3b8))
* **records:** parse floats exactly; a bad check digest no longer stops a fetch ([#317](https://github.com/ifdotpy/layr/issues/317)) ([fdcff0f](https://github.com/ifdotpy/layr/commit/fdcff0f206da5f870f6f549a2cca12f66381fa46))
* **remote:** a cache state with a missing chunk fails before its files are read, once ([#309](https://github.com/ifdotpy/layr/issues/309)) ([bb0f435](https://github.com/ifdotpy/layr/commit/bb0f4357e26de97c4eefdd1b92bae212a7a49899))
* **remote:** a sweep releases a dead chunk only after a second pass 2 hours later ([#310](https://github.com/ifdotpy/layr/issues/310)) ([475f0f9](https://github.com/ifdotpy/layr/commit/475f0f9004f8e33612f6ba0ad0542dc1bc92ce60))


### Performance

* **du:** scan btrfs metadata in bulk and size workers to the machine ([#308](https://github.com/ifdotpy/layr/issues/308)) ([071a6c6](https://github.com/ifdotpy/layr/commit/071a6c6a0d34f77c696a55cd26fe57118d238abe))
* **remote:** read record segments 32 at a time ([#313](https://github.com/ifdotpy/layr/issues/313)) ([4cc3357](https://github.com/ifdotpy/layr/commit/4cc33572df4a2fd493255d81224a87e7e3a6b259))
* **service:** run background work at lowered CPU and I/O priority ([#318](https://github.com/ifdotpy/layr/issues/318)) ([0a6ef8f](https://github.com/ifdotpy/layr/commit/0a6ef8f811b3e8fc24da1c350daced83cf3e7ca1))

## [0.31.0](https://github.com/ifdotpy/layr/compare/v0.30.1...v0.31.0) (2026-11-05)


### Features

* **remote:** clone and fetch --heads restore only the heads of the lines ([#299](https://github.com/ifdotpy/layr/issues/299)) ([319d793](https://github.com/ifdotpy/layr/commit/319d793ce75dd53315d1bde9ee5dc9957e306205))


### Bug Fixes

* **caches:** a cache saved after a change gives its source times when older caches saw other bytes ([#305](https://github.com/ifdotpy/layr/issues/305)) ([d645233](https://github.com/ifdotpy/layr/commit/d6452335f3bae07d57338ef2864b220e88f2940e))
* **checks:** a check that starts cold gives its cache back too ([#302](https://github.com/ifdotpy/layr/issues/302)) ([9bf1ca8](https://github.com/ifdotpy/layr/commit/9bf1ca8c7e52a724b22bfe029ba16e09b474571d))
* **gateway:** ask a busy gateway again ([#303](https://github.com/ifdotpy/layr/issues/303)) ([fd5f01c](https://github.com/ifdotpy/layr/commit/fd5f01c73a7425b995135a91517e85eda9a0114a))
* **remote:** the cache sweep deletes parts that no live tree needs ([#301](https://github.com/ifdotpy/layr/issues/301)) ([a6aaf03](https://github.com/ifdotpy/layr/commit/a6aaf03588437333ba63705fc448d99765353cf5))
* **trash:** an entry that another emptier deleted is no failure ([#297](https://github.com/ifdotpy/layr/issues/297)) ([c8446c5](https://github.com/ifdotpy/layr/commit/c8446c513ae1f19124b4a8c32637d7b2320022c5))


### Performance

* **remote:** an idle fetch or push reads no record segment again ([#296](https://github.com/ifdotpy/layr/issues/296)) ([34142f3](https://github.com/ifdotpy/layr/commit/34142f3125dde37d8b81e81dfb4a59a7a3bd823a))

## [0.30.1](https://github.com/ifdotpy/layr/compare/v0.30.0...v0.30.1) (2026-11-05)


### Bug Fixes

* **follow:** the prune of the background pass gets 10 min, not the 120 s of a command ([#294](https://github.com/ifdotpy/layr/issues/294)) ([4be07b4](https://github.com/ifdotpy/layr/commit/4be07b4921fb1392d5198e0edb2141cfc69bb346))

## [0.30.0](https://github.com/ifdotpy/layr/compare/v0.29.1...v0.30.0) (2026-11-05)


### Features

* **checks:** warm check copies that give their caches back, and checks run --state ([#286](https://github.com/ifdotpy/layr/issues/286)) ([8685476](https://github.com/ifdotpy/layr/commit/868547658a1a7a7303952acf9e6510b5dc593adb))


### Bug Fixes

* **caches:** take the cache saved with a line's head, and give new readers caches at push ([#285](https://github.com/ifdotpy/layr/issues/285)) ([84c590f](https://github.com/ifdotpy/layr/commit/84c590f8e81653f1bcca02f85ecc7387945a48a3))
* **push:** move cache files that a push before chunks left in parts to chunks ([#291](https://github.com/ifdotpy/layr/issues/291)) ([ea59cad](https://github.com/ifdotpy/layr/commit/ea59cadf21dc5250dd3e67dca8fcc71d92773495))
* **remote:** clones that take only the caches they use, and survive a fresh trust or a sweep ([#289](https://github.com/ifdotpy/layr/issues/289)) ([4e3b6ce](https://github.com/ifdotpy/layr/commit/4e3b6cee99acf3cb710ff2732e09bb66c4de218d))

## [0.29.1](https://github.com/ifdotpy/layr/compare/v0.29.0...v0.29.1) (2026-11-05)


### Bug Fixes

* **follow:** a build that does not finish leaves the line marked, so the next pass builds again ([#288](https://github.com/ifdotpy/layr/issues/288)) ([3046664](https://github.com/ifdotpy/layr/commit/3046664d6e53d715da89570bb53f72eac8fdb9ea))

## [0.29.0](https://github.com/ifdotpy/layr/compare/v0.28.0...v0.29.0) (2026-11-05)


### Features

* **checks:** platforms in check results, and checks pending for CI ([#272](https://github.com/ifdotpy/layr/issues/272)) ([49549dd](https://github.com/ifdotpy/layr/commit/49549dd2afc3fc1f7a21139d1f2240c6c623eee6))


### Bug Fixes

* **follow:** the pass after a local merge builds the caches of the new head ([#283](https://github.com/ifdotpy/layr/issues/283)) ([9674131](https://github.com/ifdotpy/layr/commit/967413142ef7853d6b6a5b2fca09aac08d6d1f4c))
* **push:** fail when another store writes as this machine ([#280](https://github.com/ifdotpy/layr/issues/280)) ([5fafaf9](https://github.com/ifdotpy/layr/commit/5fafaf9af9da2dbf8b12b501d78a8e6c431eeeba))
* **remote:** gc of a file store does not fail on a put in flight ([#278](https://github.com/ifdotpy/layr/issues/278)) ([8043980](https://github.com/ifdotpy/layr/commit/804398035c640654612793d0821a3ff7199c785e))

## [0.28.0](https://github.com/ifdotpy/layr/compare/v0.27.0...v0.28.0) (2026-11-05)


### Features

* **remote:** Store cache layers as content-defined chunks in pack objects ([#270](https://github.com/ifdotpy/layr/issues/270)) ([4923fc9](https://github.com/ifdotpy/layr/commit/4923fc9b6f046240859b364465158b910ee94423))


### Bug Fixes

* **follow:** a push over another remote of the same repository is a cache source ([#279](https://github.com/ifdotpy/layr/issues/279)) ([1770ddb](https://github.com/ifdotpy/layr/commit/1770ddb139bbad8d741f32f916851bea45ba3102))

## [0.27.0](https://github.com/ifdotpy/layr/compare/v0.26.0...v0.27.0) (2026-11-05)


### Features

* **remote:** a push deletes the dead cache states in the store ([#269](https://github.com/ifdotpy/layr/issues/269)) ([b508660](https://github.com/ifdotpy/layr/commit/b508660409b0f6ea43ff77e9facdc7cd1a17d410))


### Bug Fixes

* **cache:** a source file with other bytes looks newer than the taken cache ([#276](https://github.com/ifdotpy/layr/issues/276)) ([71b9091](https://github.com/ifdotpy/layr/commit/71b9091ce34158794e8854a567bdd6a3df93bb5d))
* **follow:** an up-to-date pass saves the caches when the head has none ([#275](https://github.com/ifdotpy/layr/issues/275)) ([c6611b2](https://github.com/ifdotpy/layr/commit/c6611b216b492746ccfc2ae18baa7a93102ed93f))
* **ignore:** honour .git/info/exclude and the user's core.excludesFile ([#267](https://github.com/ifdotpy/layr/issues/267)) ([fd71b50](https://github.com/ifdotpy/layr/commit/fd71b509f4adcb49d839b109cefed6cb2f01f101))

## [0.26.0](https://github.com/ifdotpy/layr/compare/v0.25.0...v0.26.0) (2026-11-04)


### Features

* **cache:** environment and toolchain in the cache key, named mismatches ([#260](https://github.com/ifdotpy/layr/issues/260)) ([dd6eb6c](https://github.com/ifdotpy/layr/commit/dd6eb6c6ef54696fe811ec0a564860eb47691086))
* **cache:** fsck --caches reports the cache data that nothing keeps ([#268](https://github.com/ifdotpy/layr/issues/268)) ([8cd57d4](https://github.com/ifdotpy/layr/commit/8cd57d458478a19bf676a0204618e8b5094098ec))
* **cache:** one cache overlay line per line and platform ([#259](https://github.com/ifdotpy/layr/issues/259)) ([e727b45](https://github.com/ifdotpy/layr/commit/e727b45879d347dd482d3a767baa9095a9ed99cb))
* **project:** delete a project without lines ([#258](https://github.com/ifdotpy/layr/issues/258)) ([fdfc8f4](https://github.com/ifdotpy/layr/commit/fdfc8f4c6a6da18f3667be0a580c1ec5504b2a74))
* **remote:** keep file times in store trees ([#262](https://github.com/ifdotpy/layr/issues/262)) ([e39850c](https://github.com/ifdotpy/layr/commit/e39850c75c2f223702ebb15e3cbd4ad088460f7b))
* **retest:** doctor names compiled-in paths and gives the run-time form ([#261](https://github.com/ifdotpy/layr/issues/261)) ([5fc91d9](https://github.com/ifdotpy/layr/commit/5fc91d981640767f89cca8568b887633c6f46227))


### Bug Fixes

* **cache:** line delete deletes the line's cache states at once ([#264](https://github.com/ifdotpy/layr/issues/264)) ([93f0054](https://github.com/ifdotpy/layr/commit/93f00548f7621c0aa6b85a5d7ee224d7084dc980))
* **follow:** one source line with an error no longer stops the cache step ([#263](https://github.com/ifdotpy/layr/issues/263)) ([144fd8d](https://github.com/ifdotpy/layr/commit/144fd8d6ed6ec0622cb0fe92bfbdbd8d8b65657f))

## [0.25.0](https://github.com/ifdotpy/layr/compare/v0.24.0...v0.25.0) (2026-11-04)


### Features

* **init:** Make a project from a Git checkout in place ([#236](https://github.com/ifdotpy/layr/issues/236)) ([ffbf676](https://github.com/ifdotpy/layr/commit/ffbf676b5fb2ba07c35c561f70a788bd76cc9d26))


### Bug Fixes

* **cache:** Make stale the cargo units that name the place of their build when a line takes a cache ([#246](https://github.com/ifdotpy/layr/issues/246)) ([eee525f](https://github.com/ifdotpy/layr/commit/eee525fe27b8abdc83216239c7c7ebfa88faf3f4))
* **cli:** Name a full disk when a command fails on it ([#249](https://github.com/ifdotpy/layr/issues/249)) ([776617c](https://github.com/ifdotpy/layr/commit/776617cf1484cde9e39e9f26c6ff64c1772cf837))
* **follow:** an untracked file does not skip the pass ([#257](https://github.com/ifdotpy/layr/issues/257)) ([77ba782](https://github.com/ifdotpy/layr/commit/77ba782ed2b2c63531b238b3325a4bb331b1784a))
* **follow:** an untracked file does not skip the pass; init accepts a root-only cache rule ([77ba782](https://github.com/ifdotpy/layr/commit/77ba782ed2b2c63531b238b3325a4bb331b1784a))
* **git:** Write the .git folder of a line again when a killed command left its old index ([#250](https://github.com/ifdotpy/layr/issues/250)) ([f54ca0f](https://github.com/ifdotpy/layr/commit/f54ca0f852052ba4c2f8484e16d2dbbb9d5c1c3d))
* **replay:** stop the chain at the upstream's states after a merge of it ([#253](https://github.com/ifdotpy/layr/issues/253)) ([5d8d09c](https://github.com/ifdotpy/layr/commit/5d8d09c13c5695d419ecb57836cd5e5b1c563b98)), closes [#251](https://github.com/ifdotpy/layr/issues/251)
* **service:** finish running requests on a stop and report a request panic ([#255](https://github.com/ifdotpy/layr/issues/255)) ([41c6201](https://github.com/ifdotpy/layr/commit/41c620102fe2fa911e7710ffc5533685b43d9cdc))
* **service:** name a request's command in the project lock holder ([#254](https://github.com/ifdotpy/layr/issues/254)) ([a823059](https://github.com/ifdotpy/layr/commit/a823059f2a82dff9aada4c9f0cd1a9e4d2e1a3e3)), closes [#252](https://github.com/ifdotpy/layr/issues/252)


### Performance

* **copy:** Compare the bytes of the copied files on four threads instead of hashing each side ([#247](https://github.com/ifdotpy/layr/issues/247)) ([1b24a20](https://github.com/ifdotpy/layr/commit/1b24a20844397e6c09a459c04a179124edf61f76))
* **free:** complete bases table, queue only what no layer holds, gc checks the table ([#202](https://github.com/ifdotpy/layr/issues/202)) ([2165940](https://github.com/ifdotpy/layr/commit/2165940086586074bb33cde9fad07f26c9879b9e))
* **trash:** Empty a folder of many entries on four threads on APFS ([#248](https://github.com/ifdotpy/layr/issues/248)) ([2e709d6](https://github.com/ifdotpy/layr/commit/2e709d6d21faadbd561d38aa0c667e5919500977))

## [0.24.0](https://github.com/ifdotpy/layr/compare/v0.23.0...v0.24.0) (2026-11-04)


### Features

* **cache:** npm adapter: prune offline, warm online ([#233](https://github.com/ifdotpy/layr/issues/233)) ([5c6d73f](https://github.com/ifdotpy/layr/commit/5c6d73f3a2cfe5817a320567df65079ca1def6f2))
* **follow:** the follow pass builds the standard commands before it saves the caches ([#232](https://github.com/ifdotpy/layr/issues/232)) ([6188ebc](https://github.com/ifdotpy/layr/commit/6188ebc85992a3d24cc5c59501d023d13a8bb1f5))


### Bug Fixes

* **retest:** find cargo through rustup, name install failures, write no file on a protected line ([#237](https://github.com/ifdotpy/layr/issues/237)) ([31b3b85](https://github.com/ifdotpy/layr/commit/31b3b85732a9d2b05967fc81c8c2f7bfee8f6fbd))
* **save:** Count the lines of the saved state in the save summary on macOS ([#239](https://github.com/ifdotpy/layr/issues/239)) ([105178f](https://github.com/ifdotpy/layr/commit/105178f2bb8b2a2ba81e6189b4afc564f9b19fe7))

## [0.23.0](https://github.com/ifdotpy/layr/compare/v0.22.2...v0.23.0) (2026-11-04)


### Features

* **retest:** init sets up follow, the sweep list and warm thin caches ([#197](https://github.com/ifdotpy/layr/issues/197)) ([e5eb0c5](https://github.com/ifdotpy/layr/commit/e5eb0c523156e24553c39a9e4cb2f4f062c7e22a))


### Bug Fixes

* **git:** Import a fetched branch on first use, and update the git refs of lines after a fetch ([#231](https://github.com/ifdotpy/layr/issues/231)) ([18ab954](https://github.com/ifdotpy/layr/commit/18ab954964c1c7a804436d8d9e69a22e69ae2df8))
* **save:** Run the checks of a named save once, and finish it after the lock ([#224](https://github.com/ifdotpy/layr/issues/224)) ([520bf68](https://github.com/ifdotpy/layr/commit/520bf68e1e728f007475b3b7ecc0f39b21edcfa3))


### Performance

* **hash:** Purpose-named hashers crate; BLAKE3 for local checks, Fx for local maps ([#234](https://github.com/ifdotpy/layr/issues/234)) ([94daee4](https://github.com/ifdotpy/layr/commit/94daee49ace44e6265c21dd843fda7527077cbfc))

## [0.22.2](https://github.com/ifdotpy/layr/compare/v0.22.1...v0.22.2) (2026-11-04)


### Bug Fixes

* **cache:** prune removes target folders that no sweep command makes after a week ([#217](https://github.com/ifdotpy/layr/issues/217)) ([12a1dd9](https://github.com/ifdotpy/layr/commit/12a1dd99c1499614e82bd8d8daa95f21107d764b))
* **gc:** Free the states of deleted lines when they leave the undo ring ([#212](https://github.com/ifdotpy/layr/issues/212)) ([cdf9c9b](https://github.com/ifdotpy/layr/commit/cdf9c9ba1d998c17b6126ec13483b7b159f6c070))
* **gc:** Plan gc --repair again instead of replaying under the project lock ([#215](https://github.com/ifdotpy/layr/issues/215)) ([50bcdaf](https://github.com/ifdotpy/layr/commit/50bcdaf841a0918554db8cb7fe25a5301247e80d)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **retest:** bound cargo listings and e2e processes by a time limit ([#208](https://github.com/ifdotpy/layr/issues/208)) ([90799c7](https://github.com/ifdotpy/layr/commit/90799c75549599477fa67971066b847dc158872a))


### Performance

* **db:** Cache prepared SQLite statements ([#216](https://github.com/ifdotpy/layr/issues/216)) ([42d2407](https://github.com/ifdotpy/layr/commit/42d24076d4872de0688a0f7854e2814724efbfee)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **line:** Add only the late changes when a line delete finds its folder changed ([#218](https://github.com/ifdotpy/layr/issues/218)) ([1d9507a](https://github.com/ifdotpy/layr/commit/1d9507acc833bb83324b9c6100feddcaf45ff9f5)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **line:** Start a new line at the state it read, without a retry ([#214](https://github.com/ifdotpy/layr/issues/214)) ([49a706f](https://github.com/ifdotpy/layr/commit/49a706f92eaf55a17059b1a092c4b0907779e374))
* **save:** Snapshot the folder and compare it with the head before the lock in save --caches ([43ebdfe](https://github.com/ifdotpy/layr/commit/43ebdfe34b3f5379dc76a96387999e8c73aad0fb)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **save:** Snapshot the folder before the project lock in save --caches ([#223](https://github.com/ifdotpy/layr/issues/223)) ([43ebdfe](https://github.com/ifdotpy/layr/commit/43ebdfe34b3f5379dc76a96387999e8c73aad0fb))

## [0.22.1](https://github.com/ifdotpy/layr/compare/v0.22.0...v0.22.1) (2026-11-04)


### Performance

* **gc:** Plan gc --repair before the project lock and read overlays once ([#210](https://github.com/ifdotpy/layr/issues/210)) ([0c9ef96](https://github.com/ifdotpy/layr/commit/0c9ef9694b36aa1beef0790134aac0b23f6f9940)), closes [#194](https://github.com/ifdotpy/layr/issues/194)

## [0.22.0](https://github.com/ifdotpy/layr/compare/v0.21.0...v0.22.0) (2026-11-04)


### Features

* **store:** Move a project to another volume ([#185](https://github.com/ifdotpy/layr/issues/185)) ([eef3a84](https://github.com/ifdotpy/layr/commit/eef3a84e0dfa0df1aa2550f6f24bc1391104a3d6))


### Performance

* **line:** unseal cache copies in one pass, skip the ownership walk of a clone ([#198](https://github.com/ifdotpy/layr/issues/198)) ([380ded1](https://github.com/ifdotpy/layr/commit/380ded144b6a91c044caf973706e79cd8b903b48))
* **lock:** Name the lock holder and save a deleted line's work before the lock ([#204](https://github.com/ifdotpy/layr/issues/204)) ([62ae203](https://github.com/ifdotpy/layr/commit/62ae2033bc5d93d24d37da1be27a1cc6590342a6))
* **lock:** Plan git imports with no project lock, write them in short chunks ([b67c78d](https://github.com/ifdotpy/layr/commit/b67c78d48062892eeb812a53088e99c6fd2f6c5e)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **lock:** Skip the metadata check under the lock after a pre-lock compare ([#205](https://github.com/ifdotpy/layr/issues/205)) ([51377d4](https://github.com/ifdotpy/layr/commit/51377d469b006a8748d8a65bbfe3113d77dd5aac)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **lock:** Transfer before the project lock in git fetch, pull and push ([#206](https://github.com/ifdotpy/layr/issues/206)) ([b67c78d](https://github.com/ifdotpy/layr/commit/b67c78d48062892eeb812a53088e99c6fd2f6c5e))
* **lock:** Upload and materialize store remotes outside the project lock ([#209](https://github.com/ifdotpy/layr/issues/209)) ([a539099](https://github.com/ifdotpy/layr/commit/a539099dba46502d227b321aaa0f24c49e3d09b2)), closes [#194](https://github.com/ifdotpy/layr/issues/194)
* **lock:** Write working folders under a line lock, commit under the project lock ([#200](https://github.com/ifdotpy/layr/issues/200)) ([39446b9](https://github.com/ifdotpy/layr/commit/39446b9abf236825acd68fcaf85c78a428497732))
* **save:** Prepare a named save's index and compare before the project lock ([#207](https://github.com/ifdotpy/layr/issues/207)) ([9173c70](https://github.com/ifdotpy/layr/commit/9173c70cabafbb07f40256a6f810e5f78a75eaf6))
* **status:** Compute line overlaps in under a second in a large project ([#201](https://github.com/ifdotpy/layr/issues/201)) ([2bf3238](https://github.com/ifdotpy/layr/commit/2bf3238322f126cedf006db11e4fed8b4b60d20c))

## [0.21.0](https://github.com/ifdotpy/layr/compare/v0.20.0...v0.21.0) (2026-11-04)


### Features

* **cache:** adapters that prune cache folders to the live set ([#169](https://github.com/ifdotpy/layr/issues/169)) ([23ade85](https://github.com/ifdotpy/layr/commit/23ade85ef9b7d067c1e82af78a75283d9c9146f0))
* **storage:** Copy a project to another file system with its sharing ([#177](https://github.com/ifdotpy/layr/issues/177)) ([055238c](https://github.com/ifdotpy/layr/commit/055238c75419e0147b742fcfa45f734d31959f2b))


### Bug Fixes

* **apfs:** Treat an entry that goes away during a thin copy as a change ([#191](https://github.com/ifdotpy/layr/issues/191)) ([b50323f](https://github.com/ifdotpy/layr/commit/b50323f7df4a2363f61a907fda3404d85764c580))
* **cache:** Never swap a cache folder that a process uses ([#190](https://github.com/ifdotpy/layr/issues/190)) ([ff44c80](https://github.com/ifdotpy/layr/commit/ff44c8073fe0a7a51abc5f53f94ab28a9bdfe076))
* **cache:** report walk errors in regenerable_by_walk ([#187](https://github.com/ifdotpy/layr/issues/187)) ([826a2bd](https://github.com/ifdotpy/layr/commit/826a2bd49295a8e6fc33aa5bbbf191ed881d7d39))
* **overlayfs:** take caches as a lower layer, no copy at line new ([#184](https://github.com/ifdotpy/layr/issues/184)) ([a61c7ab](https://github.com/ifdotpy/layr/commit/a61c7ab65afac391ef22416cc9ad5e1ae321581c))
* **worktree:** Replace a hard-linked file by rename, not by a write in place ([#188](https://github.com/ifdotpy/layr/issues/188)) ([5d1fc13](https://github.com/ifdotpy/layr/commit/5d1fc13cf6054a3f200b9bb8de0e79e0a406cfc8))


### Performance

* **line:** copy warm caches once, lock only the commit of line new ([#196](https://github.com/ifdotpy/layr/issues/196)) ([7a43657](https://github.com/ifdotpy/layr/commit/7a436577ebf4d5bcee4199669b4d1f6cccf52a90)), closes [#194](https://github.com/ifdotpy/layr/issues/194)

## [0.20.0](https://github.com/ifdotpy/layr/compare/v0.19.1...v0.20.0) (2026-11-03)


### Features

* **overlay:** Remove a cache overlay; list and repair agree on lines without its cache path ([#168](https://github.com/ifdotpy/layr/issues/168)) ([776d9ca](https://github.com/ifdotpy/layr/commit/776d9ca4af9e746a70e53d50885d7710bb8b7ae4))
* **store:** Volumes: place projects on several file systems ([#176](https://github.com/ifdotpy/layr/issues/176)) ([b49c711](https://github.com/ifdotpy/layr/commit/b49c71133ca286cf2eee3ed72aa085130f44478b))


### Bug Fixes

* **cache:** keep regenerable folders out of states without save --caches ([#178](https://github.com/ifdotpy/layr/issues/178)) ([a930bc0](https://github.com/ifdotpy/layr/commit/a930bc09a832103f54a06888cb9974fcb6d6f155))
* **save:** Save every cache that --cache names ([#175](https://github.com/ifdotpy/layr/issues/175)) ([f424905](https://github.com/ifdotpy/layr/commit/f424905ded7b28d7384ba366ef31f6b5d8a0ee2a))


### Performance

* Pick only the changed paths; reset to an equal tree moves only the head ([#172](https://github.com/ifdotpy/layr/issues/172)) ([c25f574](https://github.com/ifdotpy/layr/commit/c25f5742c49301ae8de7d1146e52873a56479d81))

## [0.19.1](https://github.com/ifdotpy/layr/compare/v0.19.0...v0.19.1) (2026-11-03)


### Bug Fixes

* **daemon:** Set the store in the LaunchAgent; follow warns without a daemon ([#166](https://github.com/ifdotpy/layr/issues/166)) ([e56e3be](https://github.com/ifdotpy/layr/commit/e56e3be29510fc7598f65a6d2513526aa73d13bf))
* **undo:** Undo the newest operation, an automatic save too; pick names kept files ([#170](https://github.com/ifdotpy/layr/issues/170)) ([b22717d](https://github.com/ifdotpy/layr/commit/b22717da68fc044d3812e981f27d9b49c1745308))


### Performance

* **gc:** Check imported states with one git process; report the bytes that freeing gives back ([#179](https://github.com/ifdotpy/layr/issues/179)) ([1a553b3](https://github.com/ifdotpy/layr/commit/1a553b34e712c0c9e42a227b8ad8a9bc244d02d5))

## [0.19.0](https://github.com/ifdotpy/layr/compare/v0.18.1...v0.19.0) (2026-11-03)


### ⚠ BREAKING CHANGES

* **gc:** Free states when their last owner goes ([#135](https://github.com/ifdotpy/layr/issues/135))

### Features

* **gc:** Free states when their last owner goes ([#135](https://github.com/ifdotpy/layr/issues/135)) ([5d611f7](https://github.com/ifdotpy/layr/commit/5d611f718bc5f84903eb3006901cea5bdbf578ce))


### Bug Fixes

* **git:** Keep imported history as records and build trees from git on read ([#157](https://github.com/ifdotpy/layr/issues/157)) ([854aa70](https://github.com/ifdotpy/layr/commit/854aa7069ebefa2dc428bbba71d5e4f00eea781a))

## [0.18.1](https://github.com/ifdotpy/layr/compare/v0.18.0...v0.18.1) (2026-11-03)


### Bug Fixes

* **build:** Take the build identity from the environment, never from .git ([#159](https://github.com/ifdotpy/layr/issues/159)) ([76c1fd6](https://github.com/ifdotpy/layr/commit/76c1fd6e52b94e300f58bcf44a1cbc9733865e41))
* **undo:** Point the line's .git at the restored head ([#163](https://github.com/ifdotpy/layr/issues/163)) ([7dddfd7](https://github.com/ifdotpy/layr/commit/7dddfd721c0d715b1414caf3f6c05c36b750721c))

## [0.18.0](https://github.com/ifdotpy/layr/compare/v0.17.0...v0.18.0) (2026-11-03)


### Features

* **retest:** doctor flags build scripts that watch .git ([#160](https://github.com/ifdotpy/layr/issues/160)) ([bb0b37b](https://github.com/ifdotpy/layr/commit/bb0b37b6f427cf4ee4978cdd70505ed802761e38))


### Bug Fixes

* **init:** Leave no partial project after a failed init ([#155](https://github.com/ifdotpy/layr/issues/155)) ([f1580ad](https://github.com/ifdotpy/layr/commit/f1580add4ceb776d87f7f1f8b3f25501ec9af61d))
* **retest:** keep incremental sessions and their locks for live units; doctor checks every profile ([#162](https://github.com/ifdotpy/layr/issues/162)) ([34c2780](https://github.com/ifdotpy/layr/commit/34c278049f38b8f42df426daa44e14a2c36ab096))

## [0.17.0](https://github.com/ifdotpy/layr/compare/v0.16.2...v0.17.0) (2026-11-03)


### Features

* layr follow, and the daemon on macOS ([#148](https://github.com/ifdotpy/layr/issues/148)) ([1aaf38d](https://github.com/ifdotpy/layr/commit/1aaf38de979823c0c3f258134015cacfaa622445))
* **retest:** init sets up a project, one build env, and a gate runner ([#149](https://github.com/ifdotpy/layr/issues/149)) ([7c3b58c](https://github.com/ifdotpy/layr/commit/7c3b58cd7d9be7147c6f4646ec58f5c14c3e412b))


### Bug Fixes

* **daemon:** Do not log a closed connection after hello as a bad request ([#156](https://github.com/ifdotpy/layr/issues/156)) ([0686bff](https://github.com/ifdotpy/layr/commit/0686bff8fb5625b359f3a7a431347c956657232e)), closes [#154](https://github.com/ifdotpy/layr/issues/154)
* **gc:** Evict only restorable states from a current store copy ([#120](https://github.com/ifdotpy/layr/issues/120)) ([2edb169](https://github.com/ifdotpy/layr/commit/2edb169bcbc2cea81b74c0e507989180f2fd4eb7))
* **retest:** sweep removes everything outside the live set ([#153](https://github.com/ifdotpy/layr/issues/153)) ([74394ee](https://github.com/ifdotpy/layr/commit/74394ee29eb053ae15fbbe42f69f527d8a1aac01))
* Stop undo before file changes when a state is missing; keep line paths consistent on a failed move ([#147](https://github.com/ifdotpy/layr/issues/147)) ([e121697](https://github.com/ifdotpy/layr/commit/e121697a75f8c4d4c1d1dbe7e5fb8998f0d3e0b5))

## [0.16.2](https://github.com/ifdotpy/layr/compare/v0.16.1...v0.16.2) (2026-11-03)


### Bug Fixes

* **caches:** The newest cache overlay state wins over an old cache of the state ([#145](https://github.com/ifdotpy/layr/issues/145)) ([f8a3d55](https://github.com/ifdotpy/layr/commit/f8a3d558b2139f4a14e9400fc2a30eacefe08fa2))

## [0.16.1](https://github.com/ifdotpy/layr/compare/v0.16.0...v0.16.1) (2026-11-02)


### Bug Fixes

* **caches:** A line from a state takes the cache saved with it ([#142](https://github.com/ifdotpy/layr/issues/142)) ([584b0d9](https://github.com/ifdotpy/layr/commit/584b0d9fb9a733fba06a35424416adb64bc5afd3))
* **retest:** sweep only configured dirs and never compile ([#141](https://github.com/ifdotpy/layr/issues/141)) ([a38756d](https://github.com/ifdotpy/layr/commit/a38756dcbe6bc121da725298d85fd800af7d012a))
* **store:** Read the marker again before refusing a half-made store ([#144](https://github.com/ifdotpy/layr/issues/144)) ([fd8cb5b](https://github.com/ifdotpy/layr/commit/fd8cb5bcfd95218396634a035ba9cb5f9ba11429))

## [0.16.0](https://github.com/ifdotpy/layr/compare/v0.15.0...v0.16.0) (2026-11-02)


### ⚠ BREAKING CHANGES

* **store:** Upgrade an unversioned store with the same format on open ([#132](https://github.com/ifdotpy/layr/issues/132))

### Features

* **store:** Upgrade an unversioned store with the same format on open ([#132](https://github.com/ifdotpy/layr/issues/132)) ([07b710d](https://github.com/ifdotpy/layr/commit/07b710d59c038cdcc0032128ea75a7897e3274ac))


### Bug Fixes

* **git:** Delete a removed remote's tracking refs ([#138](https://github.com/ifdotpy/layr/issues/138)) ([81928b8](https://github.com/ifdotpy/layr/commit/81928b8b4a88ed89ad48ec87ea9f52bb7c473c6f))

## [0.15.0](https://github.com/ifdotpy/layr/compare/v0.14.2...v0.15.0) (2026-11-02)


### Features

* **merge:** add --caches-only to take a line's caches ([#133](https://github.com/ifdotpy/layr/issues/133)) ([a255309](https://github.com/ifdotpy/layr/commit/a255309476846cac8fdd556bdaf3226cd204b399))

## [0.14.2](https://github.com/ifdotpy/layr/compare/v0.14.1...v0.14.2) (2026-11-02)


### Bug Fixes

* **retest:** keep coverage profiles out of the source tree ([#129](https://github.com/ifdotpy/layr/issues/129)) ([4617c4d](https://github.com/ifdotpy/layr/commit/4617c4d9195c3b55802f858a318541d10e718f5f))

## [0.14.1](https://github.com/ifdotpy/layr/compare/v0.14.0...v0.14.1) (2026-11-02)


### Bug Fixes

* **init:** Read the source as the caller and write the store as root ([#128](https://github.com/ifdotpy/layr/issues/128)) ([2d7d575](https://github.com/ifdotpy/layr/commit/2d7d57522e57913b355855afad68be3f57c24a01))

## [0.14.0](https://github.com/ifdotpy/layr/compare/v0.13.1...v0.14.0) (2026-11-01)


### Features

* **save:** Name the files a save leaves out ([#123](https://github.com/ifdotpy/layr/issues/123)) ([9402906](https://github.com/ifdotpy/layr/commit/94029062978bb0c2165c549900392d8b83893166))


### Bug Fixes

* **daemon:** read the busy answer when the hello write fails ([#124](https://github.com/ifdotpy/layr/issues/124)) ([14fea72](https://github.com/ifdotpy/layr/commit/14fea722ef636ac2da9da3754823f3b0a99e498d))


### Performance

* **status:** Compare the live folder once for all overlap bases ([#125](https://github.com/ifdotpy/layr/issues/125)) ([9e105ed](https://github.com/ifdotpy/layr/commit/9e105ed992b6bd3355c64800ff75a8253a34de8e))

## [0.13.1](https://github.com/ifdotpy/layr/compare/v0.13.0...v0.13.1) (2026-11-01)


### Bug Fixes

* **db:** Rebuild derived tables in one transaction ([#122](https://github.com/ifdotpy/layr/issues/122)) ([ee03be1](https://github.com/ifdotpy/layr/commit/ee03be12490789a9d777dc67494bcbbb0211c6b9))
* Remove temporary data on failure paths ([#117](https://github.com/ifdotpy/layr/issues/117)) ([c6434f9](https://github.com/ifdotpy/layr/commit/c6434f94c78c529a162e82fa4da517b0c80eec76))


### Performance

* **status:** Cache overlap compares of saved states ([#118](https://github.com/ifdotpy/layr/issues/118)) ([2ee9c62](https://github.com/ifdotpy/layr/commit/2ee9c6204f13afb0ef75552bd3e888070f70c508))

## [0.13.0](https://github.com/ifdotpy/layr/compare/v0.12.0...v0.13.0) (2026-11-01)


### Features

* **gc:** Track state owners in shadow mode ([#115](https://github.com/ifdotpy/layr/issues/115)) ([82e1850](https://github.com/ifdotpy/layr/commit/82e18503e6dcc39597d6551c98fe7664762ae780))
* **retest:** sweep cargo targets to the live unit set ([#107](https://github.com/ifdotpy/layr/issues/107)) ([00ab356](https://github.com/ifdotpy/layr/commit/00ab356fcc17d2734bf986dfe174f14254155086))


### Bug Fixes

* **gc:** Evict states only with gc.evict on ([#119](https://github.com/ifdotpy/layr/issues/119)) ([ab23eee](https://github.com/ifdotpy/layr/commit/ab23eee92e1a39ad6eed7e8ea0b7457d0d4ebb32))
* **retest:** keep the project rustflags ([#114](https://github.com/ifdotpy/layr/issues/114)) ([50d223d](https://github.com/ifdotpy/layr/commit/50d223d518b3bbeedefb68132e3b24c10a515c0d))

## [0.12.0](https://github.com/ifdotpy/layr/compare/v0.11.0...v0.12.0) (2026-11-01)


### Features

* **gc:** Keep only states that cannot be fetched back ([#111](https://github.com/ifdotpy/layr/issues/111)) ([a7b7162](https://github.com/ifdotpy/layr/commit/a7b7162a2d3ed3df1bface4a9942523847ff30a3))

## [0.11.0](https://github.com/ifdotpy/layr/compare/v0.10.0...v0.11.0) (2026-11-01)


### ⚠ BREAKING CHANGES

* **gc:** run dedup only with --dedup ([#108](https://github.com/ifdotpy/layr/issues/108))

### Bug Fixes

* **gc:** run dedup only with --dedup ([#108](https://github.com/ifdotpy/layr/issues/108)) ([aed868e](https://github.com/ifdotpy/layr/commit/aed868ece9002f4d9e57082b8d798ca5a0142735))
* **save:** Keep cache folders out of new states ([#109](https://github.com/ifdotpy/layr/issues/109)) ([505bf32](https://github.com/ifdotpy/layr/commit/505bf3282ae56dbb2b0a732ced966c7c3911fa98))

## [0.10.0](https://github.com/ifdotpy/layr/compare/v0.9.3...v0.10.0) (2026-11-01)


### Features

* **btrfs:** Refuse stores that a root snapshot rollback would empty ([#105](https://github.com/ifdotpy/layr/issues/105)) ([00592a3](https://github.com/ifdotpy/layr/commit/00592a300941ded6895832903600983f19e1291c))
* **retest:** doctor flags absolute rerun-if-changed paths ([#102](https://github.com/ifdotpy/layr/issues/102)) ([021aba4](https://github.com/ifdotpy/layr/commit/021aba4364d90f6f81edffbe91d01d027a6e5b66))


### Bug Fixes

* **gc:** Drop automatic states that differ from their parent only in ignored files ([#104](https://github.com/ifdotpy/layr/issues/104)) ([b114949](https://github.com/ifdotpy/layr/commit/b114949686a30a9f8cca10beb9bff247cbfc6a74))
* **overlays:** Delete a line's cache overlay lines with it; no present-but-missing states ([#106](https://github.com/ifdotpy/layr/issues/106)) ([5e72027](https://github.com/ifdotpy/layr/commit/5e72027a6ce4dd719f36eab5d00aedcf87fc401f))

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
