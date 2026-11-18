# Cadence engine protocol

How Cadence talks to `cadence-engine.exe`, the Music Tool's logic running with no window inside Cadence. Protocol **5** (protocol 1, plus Get music in protocol 2, Health, repairs and Organize in protocol 3, Recs in protocol 4, and Backup in protocol 5, below). The engine side is `src/service/` in this project; Cadence's side is `src/Cadence.Core/Engine/` in Cadence (`EngineHost`, plus `StandInEngine` for tests and checks). Change both and this file together.

## The program

- `cadence-engine.exe` is built from this project: `cargo build --release --bin cadence-engine`. It shares the library with the Music Tool's window (`src/lib.rs`); `src/bin/cadence-engine.rs` only calls `service::run`.
- It has no window and no console. Cadence starts it with its standard input and output as pipes. Standard error is the engine's log; Cadence writes it to `<data>\Tools\engine.log`.
- Cadence finds it at `engine\cadence-engine.exe` beside `Cadence.exe`, or wherever `CADENCE_ENGINE_EXE` says (tests, development).
- Nothing starts it at launch. The first request does.

## Environment

| Variable | Set by | Meaning |
|---|---|---|
| `CADENCE_DATA_DIR` | Cadence, always | Cadence's data folder. The engine keeps everything in `<CADENCE_DATA_DIR>\Tools`: settings (`config.json`), caches, health reports, the helpers it fetched (`tools\ffmpeg`, `slskd\bin`). Never the Music Tool's folder, never beside the exe (an update replaces that folder). Without it: `%LOCALAPPDATA%\CadencePlayer\Data`. |
| `CADENCE_OFFLINE_TEST` | passed on from Cadence | A test run: nothing is fetched and no settings are saved (both answer `offline_test`). A quick check still runs; it only reads the folder it is given and writes its report and caches in the engine's folder. |
| `CADENCE_NO_SOUND` | passed on from Cadence | Reported back in the handshake. The engine has nothing that plays sound (ffplay is never unpacked). |

## Messages

One JSON object per line, UTF-8, ending in `\n`. A line Cadence sends may be at most 1 MB.

**Request** (Cadence to engine): a whole-number `id`, unique among requests still running, a `method`, and `params` (an object; may be left out).

```json
{"id": 4, "method": "health.quick", "params": {"folder": "X:\\Music"}}
```

**Result** (engine to Cadence), once per request:

```json
{"id": 4, "result": {"files": 6, "albums": 2, "artists": 1, "issues": 3}}
```

**Error**, instead of a result. `message` is a sentence a person can read as it is; `code` is for Cadence to act on. An error about a line that could not be read at all has `"id": null`.

```json
{"id": 4, "error": {"code": "not_found", "message": "The folder X:\\Music could not be found."}}
```

**Progress event**, any number before a job's result or error:

```json
{"id": 4, "event": "progress", "data": {"phase": 1, "step": "Reading tags", "done": 120, "total": 480}}
```

Requests that take a while (jobs: `helpers.fetch`, `health.quick` / `health.check`, `soulseek.sign_in`, `music.search`, `music.get`, `music.put_in`, `repair.apply`, `history.undo`, `history.clear_old`, `tags.edit`, `organize.preview`, `organize.run`, `organize.album`, `recs.refresh`, `backup.run`, `backup.check`, `backup.repair_damaged`, `backup.eject`) run at the same time as anything else. `soulseek.status`, `soulseek.sign_out`, `music.forget`, `health.report`, `repair.plan`, `history.list`, `recs.get`, `recs.artist`, `backup.status`, `backup.drives`, `backup.preview_folders`, `backup.confirm_folders` and `backup.schedule` are answered on a thread of their own too (they may wait on the Soulseek client), so their answers can come after later ones; they are not jobs and cannot be cancelled. The others are answered in order at once.

### Error codes

| Code | When |
|---|---|
| `bad_request` | Not a JSON object, no id, no method, an id still in use, missing or wrong `params`, a setting that does not exist or a value of the wrong kind. |
| `not_ready` | Anything but `handshake` before a handshake was accepted. |
| `version_mismatch` | The handshake asked for another protocol. Cadence stops the engine and does not start it again this session. |
| `unknown_method` | A method this engine does not have. |
| `offline_test` | Refused because `CADENCE_OFFLINE_TEST` is set. |
| `not_found` | The folder to check does not exist. |
| `missing_helper` | The work needs a helper (ffmpeg) that is not there. |
| `busy` | That helper is already being fetched, or a Soulseek sign-in is already under way; or another change to the library is still running in the engine ("Another change to your library is still finishing. Try again in a moment.", see "One writer at a time"). |
| `no_login` | Soulseek needs a login and none is kept. |
| `no_helper` | The Soulseek client (slskd) is not there yet. |
| `offline` | Soulseek could not be reached (or the client would not start). |
| `sign_in_refused` | Soulseek did not take the name and password. |
| `no_music_folder` | The setting `music_dir` is empty, or that folder is not there. |
| `doubtful` | The importer would leave this copy for a person to decide; the download is kept (`music.put_in`, `music.forget`). |
| `cancelled` | The job was stopped by `cancel` (or because Cadence went away). |
| `failed` | It did not work; the message says why (a damaged settings file, a download that did not match, a fault inside a job). The engine goes on. |

Cadence's host adds two of its own, never sent by the engine: `engine_stopped` (the engine ended while a request was waiting; the request is not sent again by itself) and `no_answer` (it started but did not answer the handshake in 15 seconds), plus `missing` (the exe is not there).

## Methods

### `handshake`
`{"protocol": 5, "host": "Cadence 0.30.0"}`. Must be first. The protocol must equal the engine's or the answer is `version_mismatch` ("This engine speaks protocol 5 and Cadence asked for 4. The engine does not match this Cadence; reinstalling Cadence puts matching parts back."). Result:

```json
{"engine": "cadence-engine", "version": "1.0.8", "protocol": 5, "pid": 1234,
 "folder": "C:\\...\\CadencePlayer\\Data\\Tools", "offline_test": false, "no_sound": false,
 "methods": ["handshake", "ping", "cancel", "shutdown", "settings.get", "settings.set", "helpers.status", "helpers.fetch", "health.quick",
             "soulseek.status", "soulseek.sign_in", "soulseek.sign_out", "music.search", "music.get", "music.put_in", "music.forget", "music.kept",
             "health.check", "health.report", "health.ignore", "replace.want", "repair.plan", "repair.apply", "history.list", "history.undo",
             "history.clear_old", "tags.edit", "library.guard", "library.unacknowledged", "library.acknowledge",
             "organize.preview", "organize.run", "organize.status", "organize.album",
             "recs.refresh", "recs.get", "recs.artist", "recs.not_interested", "recs.mute", "recs.hidden",
             "backup.status", "backup.drives", "backup.preview_folders", "backup.confirm_folders", "backup.run", "backup.check",
             "backup.repair_damaged", "backup.eject", "backup.schedule"]}
```

### `ping`
Result `{"jobs": 0, "up_ms": 5120}`: how many jobs are running, and for how long the engine has been up.

### `cancel`
`{"id": 4}`. Result `{"cancelled": true}` when that job was running and has been told to stop; its own reply then follows, normally `cancelled`. `false` when there was no such job (finished already, or not a job). A quick check stops between files; a fetch stops at once and removes what it had fetched.

### `shutdown`
Result `{}`, then the engine stops every job, waits up to 5 seconds for them, and ends with exit code 0. Closing the engine's standard input does the same, so the engine never outlives Cadence.

### `settings.get`
Result `{"settings": {...}}`: the engine's own settings, the Music Tool's settings shape (`music_dir`, `downloads_dir`, `skip_folders`, `keep_lossless`, `mp3_quality`, ...). Never a secret: the Last.fm key is not part of what is written or sent. No file yet means the defaults. A damaged file answers `failed` and is left as it is.

### `settings.set`
`{"changes": {"music_dir": "X:\\Music", "months_back": 7}}`. Only settings that exist, each with the right kind of value; otherwise `bad_request` and nothing changes. The file is written beside the old one and renamed over it. Result as `settings.get`, with the changes. `offline_test` in a test run.

### `helpers.status`
Result:

```json
{"helpers": [
  {"id": "ffmpeg", "name": "ffmpeg", "version": "9.0.2", "size": 114768076, "found": true,
   "path": "C:\\...\\ffmpeg.exe", "state": "idle", "done": 0, "total": 0, "why": null},
  {"id": "slskd", "name": "the Soulseek client", "version": "0.26.2", ...}]}
```

`version` and `size` are the pinned download (`helpers.rs`). `found`: ffmpeg wherever the importer finds it (the PATH, the engine's `tools\ffmpeg`, WinGet's), slskd in the engine's `slskd\bin` only. `state`: `idle`, `getting` (`done` of `total` bytes), `unpacking`, or `failed` (`why`).

### `helpers.fetch` (job)
`{"helper": "ffmpeg"}` or `"slskd"`. Fetches the pinned version over https, checks its size and SHA-256, unpacks it beside where it goes, then moves it into place; a download that does not match is thrown away and nothing in place is touched. One already found is not fetched again. Progress `{"stage": "getting", "done": 1048576, "total": 114768076}` (and `"unpacking"`). Result `{"helper": "ffmpeg", "fetched": true, "path": "..."}` (`fetched: false` when it was already there). `offline_test` in a test run, `busy` if it is already being fetched.

### `health.quick` (job)
`{"folder": "X:\\Music", "skip_folders": "Compilations, Podcasts"}` (`skip_folders` optional: top folders that are not artists). The quick check: every track's tags and the library rules, no decoding, no online lookups, for a folder laid out as artist folders holding album folders. It only reads the folder. Progress `{"phase": 1, "step": "Reading tags", "done": 120, "total": 480}`; the steps are `Finding files`, `Reading tags`, `Checking audio`, `Looking for problems`, `Comparing track lists`, `Done`. Result:

```json
{"folder": "X:\\Music", "files": 480, "albums": 40, "artists": 1, "unchecked": 0, "issues": 12,
 "report": "C:\\...\\Data\\Tools\\health-report-1a2b3c4d5e.json", "seconds": 4.2}
```

The full findings are in `report` (the Health page's shape, described at the top of `health.rs`). Needs ffmpeg (`missing_helper` otherwise).

## Cadence's host

`EngineHost` (Cadence.Core):

- Starts the engine on the first request and sends the handshake; a start without an answer in 15 seconds is ended and counts as a crash.
- Watches it. When it ends without being asked, every waiting request fails with `engine_stopped` and it is started again after 0.5, 1, 2, 5, 10, then 30 seconds for crashes in a row (a run of a minute resets that). Five crashes within 3 minutes and it is not started again until Cadence restarts. A protocol mismatch or a missing exe is not retried at all.
- Cancels: a cancelled request sends `cancel` and waits for the job to say it stopped (at most 5 seconds more), then throws `OperationCanceledException`.
- Stops it when Cadence closes and before an update restart: `shutdown`, then ends it (and anything it started) if it is still there after 4 seconds.
- `StandInEngine` speaks this protocol over real pipes inside Cadence's process, makes up its answers, touches no file or network, and can crash, hang, go missing or speak another protocol on request. Cadence's tests use it for every path; `CADENCE_ENGINE_STANDIN=1` makes a running Cadence use it, for rendered checks.

## Protocol 2: Get music (stage 3)

Protocol 2 keeps every message and method of protocol 1 and adds the ones below. `PROTOCOL` in `src/service/mod.rs` and `EngineHost.Protocol` in Cadence both become 2.

### Asks: the engine asking Cadence

New in protocol 2: the engine can ask Cadence something and wait for the answer. An ask has `ask` (a whole number the engine picks) instead of `id`; Cadence answers with `answer`:

```json
{"ask": 3, "method": "library.hold", "params": {"reason": "Saving Blonde by Frank Ocean"}}
{"answer": 3, "result": {"hold": "h3"}}
{"answer": 3, "error": {"code": "failed", "message": "..."}}
```

| Ask | Cadence does | Answer |
|---|---|---|
| `library.hold` `{reason}` | Waits for a library sync that is running to finish, then holds the folder watcher (`HoldLibrarySyncAsync`). | `{"hold": "<token>"}` |
| `library.release` `{hold}` | Releases that hold; a sync that was put off runs now. | `{}` |
| `library.renew` `{hold}` | The engine still holds the library: Cadence counts that hold's age from now (the 30-minute limit below starts again). An unknown token is passed over. | `{}` |

The engine asks `library.hold` before its first write into the music folder and `library.release` after its last rename there, whether the import worked or not. It waits up to 2 minutes for a hold; without one it writes nothing into the music folder and the job fails ("Cadence did not let the import start; nothing was changed."). If Cadence goes away, the hold goes with it. Cadence drops a hold older than 30 minutes on its own, with a log line. So the engine never holds the library for 30 minutes without saying so again: a job that runs long (Organize, a repair or Fix all, an Edit details, Backup's bring-back, an Undo after a playing-file wait) asks `library.renew` between its steps once 10 minutes have passed since the hold or its last renewal (`Parts::renew_every`). A step never takes as long as 20 minutes: the playing-file wait inside one is at most 10 (see the guard). An error answer to `library.renew` (a Cadence that does not know it) is written in the engine's log and the job goes on. Renewing was chosen over releasing and asking again between batches: a repair's moves are told at its end, so a release in its middle would let a library sync see moves Cadence has not heard of.

An answer whose number nothing waits for (it came after the 2 minutes) is passed over. An answer with an `error` counts as no hold. While a job waits for an answer it can still be cancelled. The 2 minutes are `Parts::ask_wait` in the engine (the tests make it 1 second).

### Soulseek

The engine runs its own slskd (the helper from `helpers.fetch`, in `<Tools>\slskd`), started when it is first needed, signed in with the Soulseek login, and ended with the engine (a Windows job object). Its downloads arrive in `<Tools>\downloads`. Nothing is shared back. The login is kept in Windows Credential Manager as `Cadence: Soulseek` (its own entry, not the Music Tool's `Bradley's Music Tool: Soulseek`). The password is never written to a file and never sent back to Cadence. The engine's own tests never reach Soulseek: they run a stand-in (`src/service/standin.rs`), a small web server on this PC that answers what the engine asks slskd.

`soulseek.status` returns `{"state": "ready" | "no_login" | "no_helper" | "starting" | "offline", "name": "user or null", "words": "a sentence for the page"}`. With a login kept and slskd not running, it starts slskd in the background and answers `starting` at once, so slskd is warm by the time the page asks for a search (Cadence asks for the status when the Get music page opens). Measured with the real slskd (started with `--no-connect`, so it never reached Soulseek): the status answered in 76 ms; slskd then took 2 to 3.6 CPU seconds over about 4 seconds, peaked at about 155 MB and 42 threads, and sat idle at about 145 MB (57 to 60 MB private), 36 threads and well under 1% of one core. The engine itself idles at 12 MB, 5 threads and no CPU. A kept login that Soulseek no longer takes is `offline`, with words that say to sign in again. In a test run it starts nothing.

`soulseek.sign_in` (job) takes `{"name": "...", "password": "..."}`. Soulseek makes a new account the first time a free name signs in, and a name that is taken only signs in with its own password. The engine checks the name and password (`slskd::check_login`; one that cannot be kept is `bad_request`), (re)starts slskd with them and waits up to 45 seconds for it to be signed in, and only then keeps them. Cancel stops the wait and keeps nothing. It returns `{"name": "..."}`. Errors:
- `sign_in_refused`: "Soulseek did not take that name and password. The name may belong to someone else, or the password is not the one it was made with."
- `offline`: Soulseek could not be reached.
- `no_helper`: slskd is not there yet.
- `busy`: another sign-in is under way.
- `offline_test`.

A login that was refused, or that could not be tried, is not kept; the login kept before stays, and slskd is started with it again when it is next needed. How a refusal is seen: slskd writes Soulseek's own words ("The server rejected login attempt: INVALIDPASS") to its log, which the engine reads (`slskd::login_refused`). This has only been tried against the tests' stand-in, not the real Soulseek server.

`soulseek.sign_out` takes `{}`. It removes the kept login and stops slskd, and returns `{}`. `offline_test` in a test run.

**Idle stop.** The engine ends slskd once it has had nothing to do for `soulseek_idle_minutes` (20; 0 never stops it): no `music.search`, `music.get`, `music.put_in` or `soulseek.sign_in` running, nothing in slskd's list still waiting or arriving, and no `soulseek.status` asked in that time. It looks every few seconds (at most 5). The next `soulseek.status` or job starts it again, as the first time (the status answers `starting` in about 76 ms; slskd is ready once it has signed in to Soulseek, a few seconds, not measured against the real network). The stop and every start are written to the engine's log (standard error): "the Soulseek client had nothing to do for 20 minutes and was stopped" and "starting the Soulseek client". Cadence starts it by asking the status when the Get music page opens, not when Cadence starts (slskd holds about 145 MB while it runs).

### Getting an album

`music.search` (job) takes `{"artist": "...", "album": "...", "filter": "optional words, as the Music Tool's filter box", "fresh": false}` (`filter` and `fresh` may be left out). It runs the Music Tool's staged search and ranking (`slskd::candidates`, its filters). Copies stream in as they arrive: each progress event carries the copies not sent before, in the shape below (`best` is always false in them), and how many have been sent in all:

```json
{"copies": 12, "new": [{"copy": "c7", "user": "...", ...}]}
```

It returns the whole list, ranked:

```json
{"query": "Frank Ocean Blonde", "copies": [
  {"copy": "c1", "user": "...", "folder": "Frank Ocean - Blonde (2016) [FLAC]", "format": "FLAC", "tracks": 17,
   "bytes": 432013312, "free_slot": true, "queue": 0, "speed": 3400000, "score": 1416,
   "whole": true, "missing": 0, "best": true}],
 "cached": false}
```

- `copy` names a copy for `music.get` for as long as the engine runs.
- `whole` and `missing` compare the copy's tracks with the album's track list when one is known (`dl_facts`), and are left out when not.
- `best` marks the one copy the Music Tool would pick: the top of the whole ranked list, and only when the filter keeps it (as the Music Tool shows "Best match"). Every copy has `best`, false on the others.
- With `prefer_starts_now` off, a free slot does not lift a copy above better ones.
- The same artist, album and filter (and the same `prefer_starts_now`) asked for again within 10 minutes is answered from the engine's memory at once, with the same copies and `"cached": true`. `"fresh": true` always searches again.
- Errors: `no_login`, `no_helper`, `offline`, `sign_in_refused` (the kept login), `failed` (the search itself could not run), `bad_request` (neither artist nor album), `offline_test`.

`music.get` (job) takes `{"copy": "c1", "artist": "...", "album": "..."}` (or `"user"` and `"folder"` in place of `"copy"`, for a kept download; see below). It downloads that copy (only the files not already kept from an earlier try) and then saves it into the music folder with the Music Tool's importer and the engine's settings: `keep_lossless` and `mp3_quality`, lyrics (`save_lyrics`), tags and cover, the `Artist\Album (Year)` folder, and `recycle_originals`. Every check the importer runs today still runs. Progress:

```json
{"stage": "waiting",     "words": "Waits in line (4 ahead)"}      (also "Asking <user>", "Next in line", "Waits in line")
{"stage": "downloading", "files": 13, "done_files": 8, "bytes": 140000000, "done_bytes": 86000000, "speed": 3400000}
{"stage": "saving",      "words": "Saving as 320 MP3 with tags and cover", "done": 5, "total": 13}
```

The first saving event is `"words": "Reading the download"`, while the importer reads it (before any hold).

It returns `{"result": "imported" | "already", "folder": "X:\\Music\\Frank Ocean\\Blonde (2016)", "words": "In your library now"}` (`"Already in your library"` for `already`; `folder` is null if it cannot be found afterwards). An album the library already has is `already` without a hold: nothing is written into the music folder. Errors:
- `doubtful`: the importer would leave the copy for a person to decide (the Music Tool leaves such a row unticked: a near match of an album in the library, files that are not what their names say, a doubtful lossless copy). The words say why, in the importer's words. Nothing is written into the music folder (no hold is asked for), and the download is kept: `music.put_in` puts it in anyway, `music.forget` throws it away.
- `failed`, with the importer's words. The download is kept (reason `failed`), so a later `music.get` carries on from it.
- `no_music_folder`: the setting `music_dir` is empty, or that folder is not there.
- `no_login`, `no_helper`, `offline`, `sign_in_refused`.
- `missing_helper`: ffmpeg is not there.
- `bad_request`: a copy the engine does not know (it was found by an engine that has since ended); search again.
- `offline_test`: refused at once, before any download.

Cancel:
- While waiting or downloading, the transfers are cancelled and what arrived is removed.
- While saving, the import stops before the album is put in place (it is checked between tracks and right before the album is placed), and its build folder is removed. The hold is released. The download is kept (reason `stopped`): a later `music.get` of the same copy uses the files already there and downloads only what is missing.
- Once it is in place it stays (the job then returns normally).

### Kept downloads

A download that did not end in the library is kept in `<Tools>downloads` for another try: `doubtful` (a person decides), `stopped` (cancelled while saving), `failed` (the import or the hold did not work). The list is `<Tools>kept-downloads.json`, so it outlives the engine. A cancel while waiting or downloading keeps nothing new: it removes the files that try brought (files kept from before stay).

Copy ids last one run of the engine. A kept download is named by the person who shared it and their folder's name: the `user` and `folder` of its copy. After a restart Cadence asks `music.kept` for the list and names a kept download as `{"user": "...", "folder": "..."}` wherever a `copy` is taken (`music.get`, `music.put_in`, `music.forget`). A copy found again by a new search (same user and folder) is the same kept download.

`music.kept` takes `{}` and answers at once:

```json
{"kept": [{"user": "...", "folder": "Quiet Rooms (2020) [FLAC]", "artist": "Test Artist", "album": "Quiet Rooms",
           "reason": "doubtful", "words": "why, in the importer's words", "files": 4, "bytes": 1234567, "on_disk": true}]}
```

`music.put_in` (job) takes `{"copy": "c1"}` or `{"user": "...", "folder": "..."}`, plus optional `"artist"` and `"album"` for the hold's reason. It saves a kept download into the music folder anyway: the importer's own path with every check it makes (names, the place in the library, space, two tracks on one name, replacements) except the doubt itself, between `library.hold` and `library.release`, with the same progress, result and cancel as the saving part of `music.get`. `bad_request` when nothing of that copy is kept.

`music.forget` takes `{"copy": "c1"}` or `{"user": "...", "folder": "..."}`. It sends the kept download to the Recycle Bin and takes it off the list: `{"forgotten": true}`, or `false` when nothing of it was kept.

All three are `offline_test` in a test run, except `music.kept`, which only reads.

### Settings added in protocol 2

New fields in the engine's settings (`settings.get` / `settings.set`). The Music Tool keeps today's behaviour with their defaults.

| Setting | Default | Meaning |
|---|---|---|
| `save_lyrics` | true | Save lyrics into new albums (false: none are looked up). |
| `recycle_originals` | true | Send the downloaded files to the Recycle Bin once the album is safely in. |
| `prefer_starts_now` | true | A copy that starts now ranks above better ones that wait in line. |
| `soulseek_idle_minutes` | 20 | The engine ends its slskd after this many minutes with nothing to do (0: never). A number; it may have a fraction. |

`music_dir` is set by Cadence to its own music folder. Cadence owns that choice; its Tools settings show it but do not change it.

### Removed songs

Cadence builds "Not in your library" itself and leaves out every album whose songs were removed from the library (`library_hidden`). The engine is never asked for suggestions in protocol 2, so it needs no list. When it is (Recs, stage 5), Cadence will send the list with the request; the engine never reads library.db.

## Protocol 3: Health and repair, Edit details, the library guard, Organize (stage 4)

Protocol 3 keeps every message and method of protocol 2 and adds the ones below. `PROTOCOL` and `EngineHost.Protocol` both become 3.

### One writer at a time

The engine lets one change to the music folder run at a time: `repair.apply`, `history.undo`, `tags.edit`, `organize.run`, `organize.album`, the saving step of `music.get` and `music.put_in`, and Backup's bring-back. Each takes the engine's one writer slot before it asks `library.hold` and gives it back only after `library.release`, when its work has really ended. A second one while the slot is taken is refused at once with `busy`: "Another change to your library is still finishing. Try again in a moment." Nothing is queued: `music.get` and `music.put_in` are refused the same way (the download is kept, so saving again carries on from it). This matters after a Stop: Cadence stops waiting for a stopped job after a few seconds and lets go of its own claim, but the engine may still be finishing the step in hand; until it has, an Undo of that batch (or any other change) is refused with `busy`, so an Undo never runs while the batch it puts back is still being written. Cadence shows the words and lets the person try again.

**Stop lands soon, and leaves a whole batch.** A repair or Fix all stops between findings and between the steps of one; `tags.edit` (and a repair's tag changes) stops between the small groups of files the tag editor works on at a time (4), finishing the files in hand; Organize stops between albums; Backup's bring-back between files; `history.undo` between the lines of the batch it puts back (each line wholly back and written down, or not touched; the batch is not marked undone, its `left` says "Undo was stopped part way. What was put back stays back; Undo it again to put back the rest.", and a second Undo finishes it). Every one of them answers `cancelled`, and what it did is one consistent batch in Repair history with its Undo.

### The library guard (one rule set for every writer)

Every method that changes files in the music folder (`music.get`, `music.put_in`, `repair.apply`, `history.undo`, `tags.edit`, `organize.run`, and later Backup's bring-back) goes through one guard in the engine (`src/guard.rs`). It is checked before the hold is asked for, and again right before each change:

| What | The guard's answer |
|---|---|
| The music folder is on a network drive (`GetDriveTypeW` is `DRIVE_REMOTE`, or a `\\server\share` path) | Refused with `guarded`: "Your music is on a network drive. Cadence only reads it there; repairs and moves are off." Checks still run. |
| On a removable drive (`DRIVE_REMOVABLE`) | Allowed, but before each change the drive must still be there (next row). |
| The music folder, or the album's folder, is gone mid-run | The job stops at once with `gone`: "The music folder went away (was the drive unplugged?). Nothing more was changed; what was done is in Repair history." What was done stays recorded and can be undone. |
| A file is read-only | That file (for Organize, that album) is left alone and listed: "Read-only: Cadence leaves read-only files alone." The flag is never cleared. A move of a folder holding a read-only file leaves the whole folder where it is. |
| A file is open in another program | Left alone and listed: "Open in another program." (the existing `locked` test) |
| A file is the one Cadence is playing | The engine asks `library.playing` (below). A change to the playing file, or a move of its album, waits until it is no longer playing, checking every 2 seconds, for up to 10 minutes (`Parts::album_wait`, as `organize.album`). Still playing then, that change is left out and listed with the words "It was still playing, so it was left as it is.": a repair or Fix all leaves out the steps that touch the file (in `problems`), an Undo leaves the lines that touch it (in `left`; the batch is not marked undone, so a later Undo finishes it), Backup's bring-back leaves that file (in `problems`), and Organize leaves that album (`why` `playing`). Organize does that album last. Progress says `{"words": "Waiting for <song> to finish playing", "waiting": true}` (once per wait). Cancel works while it waits. When Cadence cannot say what is playing (an error, no answer), the change goes on: a file the player holds open is still caught as open in another program. |

In a repair's `problems` a file left alone reads "<file name>: <the words above>"; in `tags.edit` it is `{"path", "words"}` with the file's full path (two files of one name in different folders are told apart).

The network test reads a `\\server\share` (or `\\?\UNC\...`) path by its spelling alone, so the guard never reaches the network; a drive letter is asked of `GetDriveTypeW`. The guard (`src/guard.rs`) is `Guard::for_library` (`guarded` or `not_found`), `still_there` (`gone`), `file_ok` / `folder_ok` (read-only, open) and `wait_not_playing`; Organize uses the same calls. `music.get` and `music.put_in` are refused with `guarded` before anything is downloaded, and look again right before their hold.

`library.guard` (quick) takes `{"folder": "..."}` and answers `{"state": "ok" | "removable" | "network" | "missing", "words": "..."}`, so Cadence can say "look only" before anything is asked. The words: "Cadence can make the changes you ask for here.", "Your music is on a removable drive. Keep it plugged in while Cadence changes anything.", the network sentence above, and "The music folder X could not be found."

New error codes: `guarded` (the guard, or the library setting, refuses the whole job) and `gone` (the folder went away mid-run).

### New asks

| Ask | Cadence does | Answer |
|---|---|---|
| `library.playing` `{}` | Says which file is loaded in the player (playing or paused). | `{"path": "X:\\...\\07 Reckoner.flac"}` or `{"path": null}` |
| `library.moved` `{"batch": "id", "moves": [{"from": "...", "to": "...", "folder": true}]}` | In one database transaction, remaps every track whose path is `from` (a file) or lies under `from\` (a folder) to the same place under `to`. Play counts, playlists, desks, lyrics, the removed list, cover cache keys and pending scrobbles keep their identity. A path that was already remapped is passed over, so asking twice is safe. It is only asked while the engine holds the library. | `{"remapped": 120}` |

The engine asks `library.moved` after its last move and before `library.release` (Organize asks it as it goes, in small batches: see `organize.run`). It does so for `organize.run`, for repairs that move files (`file-loose`, `artist-mismatch` with "move", `merge-artist`), and for the Undo of either.

The engine asks it whenever a batch's record has moves inside the library (`moved` lines), which is those kinds; a repair that only sends things to the undo backups (an empty folder, a duplicate) does not ask it. The Undo's ask names its batch `<id>-undo` and lists each move that went back (from where it was moved to, to where it came from), newest first. A stop of the job never cuts this ask short: the files have moved.

If Cadence cannot answer (it went away, or it answered with an error), the moves stay done and are written to `<Tools>\moves-unacknowledged.jsonl`. `library.unacknowledged` (quick) returns `{"batches": [{"batch", "moves": [...]}]}` (lines kept twice under one name come as one batch, each move once), and `library.acknowledge` `{"batch": "id"}` clears one and answers `{"acknowledged": true}` (`false` when it was not on the list). Cadence asks `library.unacknowledged` once at every start, after the handshake and before its first library sync, remaps what it gets, then acknowledges each batch. Both run in a test run too (they touch only the engine's own list).

**Backup follows every move.** Each batch's moves, from a repair, an Organize or the Undo of either, are also added to `<Tools>\backup-moves.jsonl`, one line per batch: `{"batch", "when", "moves": [...]}`. Organize writes one line per album instead, as each album's rename is done, named `<batch>#<step>` (the same name its line in the unacknowledged list has); Backup follows each name once. (The plan named only Organize; a repair that files loose tracks or merges an artist moves files the same way, so Backup is told of those too.)

### Health

`health.check` (job) replaces `health.quick`, which stays as another name for it. It takes `{"folder", "skip_folders", "deep": false, "only": ["X:\\Music\\Artist", ...]}`.
- `only` checks just these top folders and folds what it finds into the report; the rest of the report stays as it was (healthscan's `Options::only`). Cadence passes the artist folders of new albums for the automatic quick check, and runs one whole quick check at the end of the first start. Each must be a folder straight inside `folder` (`bad_request` otherwise), and a report must be there already (`failed`: "There is no report yet to bring up to date. Run a check first.").
- `deep` needs ffmpeg and decodes every track not decoded before.
- `skip_folders` left out means the engine's own setting (the same list the checks after a repair use).
- Progress and result as `health.quick`, plus `"deep"` and `"only"` (how many top folders were asked for; 0 for a whole check).
- A check never changes the library, so it runs in a test run too.

`health.report` (quick) `{"folder": "..."}` reads that library's report and answers it in Cadence's shape: grouped by album, with ignored issues marked.

```json
{"when": "2026-11-18T09:12:00-05:00", "updated": null, "deep": false, "files": 480, "albums": 40, "artists": 1,
 "deep_left": 480, "lists_left": 3, "library_changes": "repair",
 "albums_with_issues": [{"folder": "X:\\Music\\Radiohead\\In Rainbows (2007)", "artist": "Radiohead", "album": "In Rainbows",
   "issues": [{"key": "Cut off|x:\\music\\...", "severity": 1, "category": "Cut off", "words": "Reckoner stops at 4:10 of 5:10",
               "files": ["...\\07 Reckoner.flac"], "fix": "redownload", "fixes": [{"words": "Replace album", "choice": ""}],
               "fix_all": false, "ignored": false, "data": {}}]}],
 "small_things": [{"category": "Small cover art", "fix": "cover", "albums": 38, "fix_all_words": "Find bigger covers", "keys": ["..."]}]}
```

- Built by `health::engine_report`, which reads the report the way the Health page does. An album also carries `path` (the finding's own path, which the key is made from); the top also carries `cancelled`, `own` (written by the engine's own check) and `unchecked`.
- `albums_with_issues` lists the severity 1 and 2 findings, album by album, the worst albums first.
- `fixes` comes from `repair::choices`, and is empty when `repair::can_fix` is false. `redownload` gives "Replace album" (choice `""`) and, for one track, "Replace only <title>" (choice `"only"`; the title is the file's name without its extension): Cadence calls `replace.want`, then opens Get music with the search filled in (`data.search`).
- `small_things` groups severity-3 findings by category (and fix kind). `albums` and `keys` count only the ones not ignored; `ignored` says how many are. `fix_all_words` is there only when `repair::can_fix_all` (the Health page's own words, `repair::action_words`).
- With no report yet, `when` is null and the lists are empty. A report that cannot be read is `failed`.
- The report is the engine's own (`<Tools>\health-report-<id>.json`), and so is the ignore list (`<Tools>\ignored-issues.txt`).

`health.ignore` (quick) `{"keys": ["..."], "ignored": true}` adds to the ignore list, or takes from it with `false`. Answers `{"changed": 2}`. Refused with `offline_test`.

`replace.want` (quick) `{"folder", "artist", "album", "why", "files": []}` writes the album down (`replace::add`, in `<Tools>\replacements.json`, the list the engine's importer reads), so the next `music.get` of that album replaces it once the new copy passes its own check. Answers `{}`. Refused with `offline_test`.

### Repairs

`repair.plan` (quick, answered on its own thread) `{"folder": "...", "keys": ["..."], "choice": ""}` says what the repair would do, in plain words, and what stands in the way. It changes nothing.

```json
{"steps": ["Set the album artist to Radiohead in 07 Reckoner.flac"], "problems": ["07 Reckoner.flac: Open in another program."]}
```

One step per file or folder changed (`repair::Step::words`): "Move <name> to <folder>", "Move <name> into the undo backups", "Set the album artist to Radiohead in <name>" (fields set, cleared, junk, cover, length header, lyrics), "Save <name>", "Make <name> a plain MP3". A guard refusal of the whole library is one of the problems; the plan still answers. `bad_request` for a key that is no longer in the report ("That issue is no longer in the report. Run a check again."), for issues of different kinds, and for a kind with no repair.

`repair.apply` (job) `{"folder": "...", "keys": ["..."], "choice": "", "title": "Set one album artist on In Rainbows"}`.
- One key is one repair. Several keys of one fix kind are a Fix all (one batch, one Undo); only the kinds `repair::can_fix_all` allows. Without a `title`: "Fix all: <category> (<n> issues)", or "<category>: <artist> <album>".
- The findings are read from the report at the start (by key), and each is planned again right before it is carried out, so it sees the library as it is.
- Refused with `guarded` when `library_changes` is `"look"` ("Look only is on, so repairs are off. Change it in Tools settings, Library."), with `offline_test` in a test run, and by the guard.
- Order: the guard, `library.hold`, each change (the guard again, the playing-file wait), `library.moved` if anything moved, the report brought up to date for the touched top folders (a check with `only`, inside the job), then `library.release`. Without a hold nothing is changed (`failed`: "Cadence did not let the repair start; nothing was changed.").
- Progress `{"done": 3, "total": 38, "words": "Radiohead In Rainbows"}` before each finding, and `done` = `total` at the end. Result `{"batch": "20261118-091200-a1b2c3", "changed": 10, "problems": []}` (`batch` is null when nothing was changed).
- Cancel stops between findings, between the steps of one, and between groups of files whose tags change (or during a playing-file wait); the answer is then `cancelled`, and what was done is one batch in Repair history. The folder going away answers `gone` the same way.

### Repair history

`history.list` (quick) `{"folder": "..."}`, newest first:

```json
{"repairs": [{"id": "...", "title": "...", "when": "...", "changes": 10, "undone": false,
              "kind": "repair" | "edit" | "organize" | "import" | "restore", "folders": ["..."]}],
 "old": {"count": 4, "bytes": 1234}}
```

`old` is what `history.clear_old` would send to the Recycle Bin: repairs older than 30 days (`bytes`: the size of their backup folders).

- Only the batches of this library are listed (a record names its library; one that does not is listed too, as several libraries on one drive share its backups folder).
- `kind` is written into a batch's record when it starts (`undo::Batch::begin_kind`: repairs `repair`, Edit details `edit`, Organize `organize`). A record without one is told by its title: "Import:" is `import`, "Backup:" is `restore`, "Organize" is `organize`, "Edit details" is `edit`, anything else `repair`.
- `folders` are the album folders it touched (a file's folder, or the folder itself), each once, at most 20.

`history.undo` (job) `{"folder", "id"}` runs `repair::undo` under the guard and a hold, after the playing-file wait (for any place the batch touched), with `library.moved` for moves; the report is then brought up to date. Result `{"left": ["what could not be put back"]}`. Refused with `offline_test`, by the guard, and with `bad_request` for an id that is not in this library's history or was already undone. Look only refuses the Undo of a repair or an Organize, as it refuses them; the Undo of an Edit details or an import (which a person made by hand) still works.

`history.clear_old` (job, only ever from a click) `{"folder"}` answers `{"cleared": 4, "left": []}`. Refused with `offline_test`.

### Edit details through the engine

`tags.edit` (job) takes:

```json
{"folder": "...", "title": "Edit details: In Rainbows",
 "files": [{"path": "...", "set": {"title": "...", "artist": "...", "album": "...", "album_artist": "...", "genre": "...", "year": "...", "track": "...", "disc": "..."}}]}
```

- Only those eight fields (`bad_request` for any other, for a value that is not words, longer than 1000 characters or holding a line break, and for a file outside `folder`). An empty value clears the field.
- Each file goes through the Rust tag editor (`tagwrite`), which proves the audio is byte for byte the same. This is `repair`'s own `Step::Tags`: a copy is changed and checked, then takes the file's place.
- The old file goes into the undo backups, as one batch of kind `edit`, so it shows in Repair history with Undo.
- Under the guard (read-only and open files are listed, the playing file waits) and a hold. Not refused by Look only. The report is brought up to date for the artist's folder before the release, as after a repair.
- Without a `title`: "Edit details: <album folder>".
- Result `{"batch", "changed", "problems": [{"path", "words"}]}` (`path` is null for a problem that names no file).
- Cadence keeps its one-step Undo in the panel, and it calls `history.undo` for that batch.
- Refused with `offline_test`; in a test run Cadence writes nothing.

### Organize into Artist \ Album (Year)

It runs only by the person's choice, after a preview, and never on its own for albums already in the library (Keep the library organized, below, files only new arrivals, one at a time). These methods are refused with `offline_test`, by the guard, and by the library setting: organizing needs `library_changes` = `"organize"`, and any other value is refused with words that say where to change it. It is tested only on temp copies.

`organize.preview` (job) `{"folder": "...", "hidden": ["paths of tracks removed from Cadence"]}` reads tags only, and writes `<Tools>\organize-preview.json`. Result:

```json
{"preview": "p-20261118-091200", "to_move": 212, "in_place": 1931, "left": 138, "backup_extra_bytes": 0,
 "counts": {"artist": 14, "renamed": 3, "tidy": 190},
 "moves": [{"from": "X:\\Music\\radiohead - in rainbows", "to": "X:\\Music\\Radiohead\\In Rainbows (2007)", "files": 12, "sidecars": ["cover.jpg", "In Rainbows.cue"], "changes": ["tidy"]}],
 "left_alone": [{"folder": "...", "why": "missing_tags", "words": "..."}]}
```

`why` is one of `missing_tags`, `mixed_artists`, `loose_tracks`, `two_albums_one_folder`, `removed`, `read_only`, `open`, `target_exists`, `collision`, `too_long`, `changed`; and in a run also `playing` (still playing when its turn came, and the wait was stopped) and `failed` (Windows would not do the rename).

Rules (from the plan file's "Decided 2026-11-18 (later)"):
- **Tags decide.** The album artist (else the one artist every track shares), the album and the year come from tags. Names are cleaned with the importer's rules (`clean_name`, `fit_path`): ":" becomes " - ", forbidden characters become "_", trailing dots and spaces go, reserved names get a leading "_", and long names are shortened. With no year the folder is `Album`, the importer's own rule.
- **Some albums stay where they are,** listed with the reason: missing tags, several artists on an album not marked as a compilation (`mixed_artists`), loose tracks in an artist folder, a folder holding more than one album, and albums removed from Cadence.
- **Compilations have their own folder.** An album marked as a compilation (the compilation tag, or "Various Artists" as its album artist, or as the artist of tracks with no album artist) goes to `<compilations_folder>Album (Year)`. The folder name is cleaned by the same Windows rules; an empty name, or one that cannot be a folder, is "Various Artists". Changing the setting moves nothing; the next preview proposes moving the compilations to the new folder.
- **Multi-disc albums** (Disc 1 or CD2 subfolders) are one album and move as one folder.
- **Folders only.** Track file names are not changed.
- **Nothing is ever overwritten.** When two albums aim at one folder, neither moves (`collision`). A target that already exists is not moved into (`target_exists`).
- **Renames only.** Same-volume renames (`MoveFileExW` without `MOVEFILE_COPY_ALLOWED`), never a copy then a delete, and no file is opened for writing.
- **The folder's own words come along.** Words in brackets, parentheses or curly braces in the album folder's name that the album tag does not have are carried over into the new name, before the "(Year)", with the bracket style as written: `AFI\[2013] Burials (Best Buy Exclusive)` becomes `AFI\Burials (Best Buy Exclusive) (2013)`, `Funeral  [Japanese Special Limited Edition] (2004)` becomes `Funeral [Japanese Special Limited Edition] (2004)`; also "(UK Version)", "[Deluxe Edition]", "[EP]", "(Remastered 2004)". Left out: a bracketed year or number ("[2003]", which becomes the "(Year)"), a disc note ("(Disc 1)"), a note of the file format ("[FLAC]", "(320 kbps)", "[24bit-96kHz]", "[WEB]"), and words the album tag already has (compared without case or punctuation). Curly braces are written as parentheses: `(2013)The 20-20 Experience {Target Deluxe Edition}` becomes `The 20_20 Experience (Target Deluxe Edition) (2013)`. Spaces are made single. Edition words outside brackets that the album tag does not have are kept too, put in parentheses before the year: "deluxe", "edition", "remaster(ed)", "expanded", "anniversary", "special", "limited", "bonus", "collector's", "reissue", "version", "explicit", "clean", "super", "ultimate", "definitive", with the words joined to them (other such words, and ordinals like "20th"), and ALL CAPS made Title Case: `Akon\Trouble (2004) DELUXE EDITION` becomes `Akon\Trouble (Deluxe Edition) (2004)`, `[1998] Y 20TH ANNIVERSARY EDITION` becomes `Y (20th Anniversary Edition) (1998)` (and `X 20th Anniversary Edition (1999)`, whose words are already in that order, is in place). So two editions of one album go to different places; if two albums would still go to one place, neither moves (`collision`).
- **Near matches are already in place.** An album whose folder differs from where it would go only in letter case, or only in punctuation and spacing, stays and counts in `in_place`: "1000 gecs" and "1000 Gecs", "B.O.A.T.S. II Me Time" and "B.O.A.T.S. II - Me Time". So a case-only rename is never proposed. (The comparison is made part by part below the music folder, against both the full new name and a shortened one.)
- **Each move is marked** in `changes`, so Cadence can group them: `"artist"` (the artist folder changes, Andrew Jackson Jihad to AJJ; an album lying at the top whose folder name holds the artist is not marked), `"renamed"` (the folder's name and the album tag share no words, small words like "the" aside: "50 Cent\Animal Ambition" to "Don't Worry 'Bout It - Single (2014)"), or `"tidy"` alone (only the way the name is written changes: "[1999] X" to "X (1999)"). A move can carry both of the first two. There is no `"year"` mark and no `year` count: the folder's own year is always kept (below), so no move changes a year.
- **The folder's year is kept.** When the album folder's name has a year on its own in brackets or parentheses ("(1971)", "[1999] X", "X [2004]"), the new name uses that year, not the tags' (a reissue's tags often carry the reissue's year): "Alice Coltrane\Journey in Satchidananda (1971)" tagged 2009 stays as it is (in place), and "[2010] Home Style (Recordings)" tagged 2006 becomes "Home Style (Recordings) (2010)". A year inside other words ("(Remastered 2004)") is not the folder's year. The tags' year is used only when the folder's name has none. `counts` says how many moves carry each.
- **A move into a folder named like its own** (an album lying at the top in a folder named after its artist) goes through a temporary name.

`organize.run` (job) `{"folder": "...", "preview": "p-..."}`:
- Right before moving each album it reads the album folder's listing and tags signature again. An album that changed since the preview is left alone (`changed`).
- Order: the guard, `library.hold`, then for each album: checked again and moved with the engine's library lock held (so no other writer can change it between the check and the rename), a journal line written and flushed to disk BEFORE the rename (in `<backups root>\<batch>\log.jsonl`, the undo batch, kind `organize`), the rename, the move written into `moves-unacknowledged.jsonl` and `backup-moves.jsonl` (both on the disk, under the name `<batch>#<step>`), then a "done" line.
- Cadence hears of the moves as the run goes: one `library.moved` (named with the batch's id) once 10 albums have moved since the last one, or once the first of them is 30 seconds old, whichever comes first (`Parts::moves_every`, `moves_after`); also before the wait for the playing album, and at the end. What Cadence takes comes off the unacknowledged list again; what it does not take stays there for its next start. So Cadence's paths are never more than one small batch behind, and a crash at any point leaves every finished move in both lists.
- The hold is renewed between albums (`library.renew`, see "Asks: the engine asking Cadence").
- The playing album goes last, and waits for up to 10 minutes; still playing then, it stays (`playing`).
- Empty folders left behind are removed only when truly empty (no hidden files either).
- Then the last `library.moved`, the report brought up to date, and `library.release`.
- Progress `{"done": 40, "total": 212, "words": "Radiohead \\ In Rainbows (2007)"}`. Result `{"batch", "moved": 212, "left_alone": [...]}`.

**Crash-safe.** When a run starts, and when the engine starts (before any other library write), it settles any organize batch whose log has a move line without its "done" line. If the album is at `to`, it writes "done", and the move is written into `moves-unacknowledged.jsonl` and `backup-moves.jsonl` (again, harmlessly, if the run had already written it), so Cadence's next start and the next backup follow it. If it is at `from`, the line is marked not done. An album caught at its temporary name (a crash in the middle of a hop through `<name>.organizing-<hex>`) is never counted as moved: it is renamed back to `from` and marked not done; if Windows will not rename it back, the step stays open, the album is listed in `stuck`, and the next start tries again. Either way nothing is lost. A batch whose `batch.json` is torn or missing (a power cut while it was written) but whose log holds Organize's lines gets its record written again from the log (title, kind `organize`, the time from its id, no music folder), so it is settled and listed in Repair history with its Undo. Every batch's `batch.json` is now flushed to disk before its first log line. `organize.status` (quick) `{"folder", "seen": "<batch>"}` answers `{"interrupted": {"batch", "moved", "left", "stuck": ["X:\\Music\\A.organizing-1a2b3c"]}}` or `{"interrupted": null}`, so Cadence can say what happened; `seen` (optional) says Cadence has shown that batch, and it is not named again. `seen` writes in the library's undo backups, so it is refused with `offline_test` in a test run; asking without it is answered.

**Stop** finishes the album in hand and ends; what moved is one batch with Undo. **The drive going away** ends the job with `gone`; every album is then either wholly at `from` or wholly at `to`.

**Undo** (`history.undo`) moves folders back, last moved first. An album whose old place is now taken by something new stays where it is and is listed. It asks `library.moved` with the reverse moves.

**Backup follows.** Each organize album's move is added to `<Tools>\backup-moves.jsonl` as soon as it is done (one line per album, `<batch>#<step>`), and an Undo's reverse moves as one line. Backup (stage 6) renames its copies to match before it copies anything, so nothing is copied twice and a moved file never counts as deleted.

### Keep the library organized: one new album (`organize.album`)

`organize.album` (job) `{"folder": "X:\Music\radiohead - in rainbows", "library": "X:\Music"}` files one album that has just arrived by Artist \ Album (Year). Cadence calls it when an album arrives later (Get music, an import, a repair's replacement, a new folder the watcher sees) and `keep_organized` is on. `library` is optional (default: the music folder in Settings).
- Refused with `offline_test`; with `guarded` unless `library_changes` is `"organize"` and `keep_organized` is true; by the guard; and with `bad_request` when `folder` is not an album folder in that music folder.
- The same path as a run: the guard, `library.hold`, a preview of that album alone (the rest of the library is only listed, to know its artist and album folders and what is in the way), the journal, the rename, `library.moved`, `backup-moves.jsonl`, the report brought up to date, `library.release`. It is ONE Repair history entry of its own (kind `organize`), and `history.undo` puts it back, asking `library.moved` with the reverse move. A crash between its journal line and its "done" line is settled at the next start like any organize batch (see "Crash-safe").
- Never while it plays: while the player has a file of it loaded, it waits (checking every 2 seconds, progress "Waiting for <song> to finish playing") for up to 10 minutes, then leaves it with `why` `playing`. Cancel works while it waits.
- Result: `{"moved": true, "batch", "from", "to"}`; or `{"moved": false, "why", "words"}` when it stays (any `why` above, for example `target_exists`); or `{"moved": false}` when it is already in place.

The importer files the same way, so Get music needs no second move: a compilation goes to the compilations folder (its tags keep "Various Artists"), and with Keep the library organized (and `library_changes` = `"organize"`) a new album goes straight to `<Artist>\Album (Year)`, never onto a shelf ("Studio Albums") or beside the copy it replaces (that copy still goes to the undo backups, as before).

### Settings added in protocol 3

| Setting | Default | Meaning |
|---|---|---|
| `library_changes` | `"organize"` | `"look"`: Cadence changes nothing on its own: no repairs, no Fix all, no Organize, and Health offers only Ignore. What the person asks for by hand still works: Get music's imports and Edit details. `"repair"`: repairs too. `"organize"`: repairs and Organize. Cadence's Tools settings, Library page, shows these as Look only, Repairs only, and Repairs and organizing. |
| `auto_check_new` | true | Cadence runs a quick check of new albums (`health.check` with `only`). |
| `compilations_folder` | `"Various Artists"` | The folder (in the music folder) marked compilations go into, by Organize and by imports. Added with Keep the library organized. |
| `keep_organized` | false | Keep the library organized: each new album that arrives is filed by Artist \ Album (Year) at once (`organize.album`), and imports already land there. Acts only with `library_changes` = `"organize"`; "look" or "repair" turn it off, and so does this setting. Albums already in the library move only through an Organize preview the person confirms. | The engine's default is off: Cadence sends its own answer with `settings.set` (on once the person has chosen Organize), right after the handshake and before any other request, so an engine that starts first (an upgrade) files nothing on its own.

`health.report` also carries `library_changes`. `settings.set` refuses any other value for it (`bad_request`). The Music Tool's window keeps its own switch, `look_only`, and does not read `library_changes`; in the shared settings shape `look_only` set counts as `"look"` whatever `library_changes` says (`Config::library_changes()`), and so does a value the engine does not know.

**Engine start.** Before its first library write the engine settles Organize batches a crash cut off (`organize::recover_all`, see Organize, "Crash-safe"); the call goes in `serve` with Organize.

## Protocol 4: Recs (stage 5)

Protocol 4 keeps every message and method of protocol 3 and adds the ones below. `PROTOCOL` and `EngineHost.Protocol` both become 4.

Recs are the Music Tool's (`engine.rs`: new releases from MusicBrainz and Apple Music, artists to try from Last.fm's similar artists), with three changes:
- **Plays come from Cadence.** Cadence sends its own play counts per artist (all time, 12 months, 3 months, recent), so the engine no longer reads Last.fm's top artists and recent tracks. That saves four Last.fm requests of up to 1,000 rows each per refresh, and New releases works with no Last.fm at all.
- **One Last.fm sign-in: Cadence's.** The engine asks for the account and never keeps it.
- **What Cadence knows is never suggested:** albums in the library, albums removed from the library, anything marked Not interested, and muted artists.

### New ask

| Ask | Cadence does | Answer |
|---|---|---|
| `lastfm.account` `{}` | Gives its signed-in Last.fm user and its API key, or none. | `{"user": "name", "api_key": "..."}` or `{"user": null}` |

The engine asks it at the start of each refresh, uses it for that refresh only, and never writes it to a file or its log. An answer with an error, or none within the ask's wait, counts as no account. (The Last.fm addresses carry the key; the lookup cache is named by a hash of the address and holds only Last.fm's answers.)

### Methods

`recs.refresh` (job) `{"folder": "X:\\Music", "artists": [{"name": "Radiohead", "plays": 4120, "plays_12m": 400, "plays_3m": 90, "plays_recent": 12}], "removed": [{"artist": "...", "album": "..."}]}`.
- `artists` is Cadence's play counts. An artist in the library with no plays is still checked for new releases.
- It runs the Music Tool's refresh (`engine::run_with`, the same code as `engine::run`) with those plays in place of step 2: Last.fm's top artists (three periods) and recent tracks are not read. Without a Last.fm account it skips Artists to try and says so in the result (`lastfm: false`); the artists to try from an earlier refresh stay. `bad_request` without `folder`, `no_music_folder` when it is not there.
- Progress `{"step": "releases" | "similar", "done": 412, "total": 1226, "new": [release or artist rows not sent before]}`: covers fill in as they are found. Rows are sent at most every 250 ms, so one event may carry both kinds; a release row is sent again when it changed (a cover or a date filled in). `step` is the task that reported last.
- It writes `<Tools>\recs.json` (the Music Tool's results, plus `lastfm`, `folder` and `removed`) once, when the refresh ends. The covers on screen stay put during a refresh: rows are only added or updated, never dropped mid-refresh. A whole refresh then replaces the list (as the Music Tool's does); a refresh a source failed part-way, or a stopped one, keeps the rows that were there and adds its own.
- Result `{"releases": 48, "similar": 120, "lastfm": true, "generated": "...", "notes": ["a source that failed, in plain words"]}`. Cancel stops it (`cancelled`); what was found so far is kept.
- In a test run (`CADENCE_OFFLINE_TEST`) it is refused with `offline_test`; Cadence's checks use the stand-in. The engine's own tests point `Net` at a local stand-in server, never the real services.

`recs.get` (quick) `{"show_earlier": false}` reads `recs.json` and answers, leaving out Not interested, muted artists, removed albums and albums in the library (those become `"in_library": true` rows instead):

```json
{"generated": "2026-11-18T06:00:00-05:00", "lastfm": true, "newest_date": "2026-10-31",
 "releases": [{"key": "...", "artist": "Kate Bush", "title": "...", "date": "2026-11-14", "kind": "album", "art_url": "https://...",
               "plays": 412, "in_library": false, "upcoming": false, "tracks": 10}],
 "similar": [{"key": "...", "name": "Heavy Heavy", "score": 0.82, "because": ["Kate Bush", "Björk"], "photo_url": "https://...", "url": "https://www.last.fm/music/...", "listeners": 120000}]}
```
- `in_library` is checked against the music folder of the last refresh as it is now (folder and file names only, no tags), so an album imported since shows as in the library. `show_earlier` false leaves out releases before the month `months_back` (9) months ago. `similar` leaves out Not interested and muted artists; `score` is scaled so the best is 1, `photo_url` is null (Last.fm has no artist pictures; Cadence uses its own), and each row also carries `tags`. `generated` is null until a refresh has finished.

`recs.artist` (quick, answered on its own thread) `{"name": "First Aid Kit", "owned": ["album titles in the library"]}`: the artist page's part: `{"new": release or null, "try": [similar rows, at most 8], "covered": true}`. Answered from `recs.json` alone; it never asks the network (the plan said it would for an artist the last refresh did not cover; that lookup is not built yet). `covered` is false for such an artist. `try` are the artists to try that name this artist among their reasons.

`recs.not_interested` (quick) `{"key": "...", "on": true}` and `recs.mute` (quick) `{"artist": "...", "on": true}` keep their lists in the engine's settings, in the Music Tool's own fields: `dismissed` (release keys, and `artist|<key>` for an artist to try) with `dismissed_names` (each key's artist and title, so a key can be listed after its row has left the recommendations), and `muted` (artist key to name). So Settings, Library can list them for Bring back: `recs.hidden` (quick) answers `{"not_interested": [{"key", "artist", "title"}], "muted": ["..."]}`. All three writes are refused with `offline_test`. A muted artist is also left out of the next refresh's lookups.

### When it refreshes

Cadence starts `recs.refresh` by itself once a day: at its start when `recs.json` is older than 24 hours, else when it is 24 hours old, but only while nothing else in Tools is running and the PC is not on battery saver. "Refresh" on the page is a quiet word. The first refresh after the guided first start is the first look ("Looking for new albums").

## Protocol 5: Backup (stage 6)

Protocol 5 keeps every message and method of protocol 4 and adds the ones below. `PROTOCOL` and `EngineHost.Protocol` both become 5.

Backup is the Music Tool's own backup worker (`src\backup\native.rs`), started through `src\backup\api.rs`; the service is `src\service\backup.rs`. Every safety rule stays where it was, in `native.rs`. What changes for Cadence:
- **Its own settings and logs**, in `<Tools>\backup` (`config.json`, the confirmed folders; `logs\backup-log.txt` and `logs\backup-log-second.txt`). Never the Music Tool's backup folder, never its settings.
- **Cadence's own data is saved beside the music**, and nothing else: `<backup>\_Cadence\<date>\Data`, from Cadence's data folder (the parent of `<Tools>`). Its databases (`*.db`) go through SQLite's online snapshot (`backup\sqlite.rs`), which reads committed WAL pages without touching the live files; `-wal`, `-shm`, `-journal` and `.tmp` files are never copied. Left out: `Cache`, `Artwork`, `Logs`, `Models`, `Heard`, `UpdateDownloads`, and in `Tools` the helpers, caches, downloads, import work and backup logs (`Tools\slskd`, `Tools\tools`, `Tools\downloads`, `Tools\cache`, `Tools\import-work`, `Tools\backup\logs`), `engine.log` and `library-changing.lock`. The Music Tool's data and the code are not saved (the Music Tool's own backup still saves them). The last 30 dated copies are kept.
- **Moves follow** (below), so an Organize or a repair that moved albums costs no copying.
- **Bring back** writes into the library through the guard, a hold and an undo batch of kind `"restore"`.
- **The nightly task is Cadence's own** (`Cadence Backup (nightly)`), made only from a click.

There are two backups, as in the Music Tool: `"which": "main"` (a drive that stays at its letter, backed up every night) and `"which": "second"` (a drive plugged in by hand, found by its marker whatever its letter). `which` may be left out; it means `main`.

### Safety rules, and where each is kept

| Rule | Where |
|---|---|
| Only an explicit confirm enrolls a library and backup folder pair. Nothing else binds folders, not a run and not a preview. | `native::confirm_folders` (`bind`), called only by `backup.confirm_folders` with `"confirm": true` |
| Each folder is known by its identity: its volume serial and file ID (`directory_id`), plus a marker file (`_backup-id.txt`) in the backup folder. A different disk at the same letter (another name, or the same name with another volume serial), a replaced folder, or a copied marker: nothing is copied or moved until the person confirms again. | `native::validate_binding`, before a run, before every copy and every rename, and before the settings are written; the disk's name in `select_dest` |
| Every copy is read back and compared with the library before it takes the old copy's place. | `native::copy_checked` |
| Nothing is deleted from the backup. A replaced copy moves to `_changed\<stamp>`, a removed file to `_deleted\<stamp>`; both are kept 30 days, then go to the Recycle Bin. More than 200 removed files (or more than a fifth of the backup) at once stops the run (`too_many_removed`) until the person asks for `"kind": "mirror"`. | `native::run`, `install_file`, `clean_history` |
| The library is only read (each file is opened so no one can write it while it is copied). | `native::locked_read` |
| One writer per backup folder: the same named lock as the Music Tool's worker, so the nightly task, a run from Cadence and the Music Tool never write one folder at once (`busy`). | `native::RunLock` |
| Links, junctions, `..` and the backup's own folder names are refused. | `native::plain_path`, `below`, `reserved` |
| Every write refuses a test run (`offline_test`): a test run never copies, binds, ejects or schedules. | `service\backup.rs` (`run_check`, `confirm_folders`, `repair_check`, `eject_check`, `schedule`, `nightly`) |

### Methods

`backup.status` (quick, own thread) `{"folder": "X:\\Music"}`. Only reads (the settings, the logs, the drives, Task Scheduler).

```json
{"state": "not_set_up" | "good" | "needs_look",
 "sentence": {"words": "Backed up 3 hours ago", "loud": "3 hours ago"},
 "line": "the one line under it",
 "alerts": [{"words": "No backup has finished for 6 days. The last one was Nov 11 at 3:00 AM.", "serious": true}],
 "main": card, "second": card,
 "schedule": {"on": true, "known": true, "words": "Every night at 3:00 AM, even when Cadence is closed", "at": "3:00 AM",
              "next": "Tomorrow at 3:00 AM", "running": false, "problem": null},
 "running": [{"which": "main", "kind": "backup"}]}
```

A card:

```json
{"set_up": true, "folder": "F:\\Backups",
 "drive": {"state": "here" | "away" | "wrong_disk" | "needs_confirm" | "none", "name": "Backup drive", "letter": "D:",
           "free": 1319413953331, "total": 1979120929996, "words": ""},
 "last": {"when": "2026-11-18T03:00:00", "words": "Today at 3:00 AM", "result": "Copied 12 files (84 MB) in 9.1s", "tone": "good",
          "brief": "2 new albums  ·  All 12 copies checked  ·  Cadence saved", "by": "nightly"},
 "last_finished": "2026-11-18T03:00:41", "last_finished_words": "3 hours ago",
 "nights": ["done", "done", "missed", "problem", "...14 in all, oldest first"],
 "nights_by": ["nightly", "hand", null, "nightly", "...one for each night"],
 "check": {"when": "...", "words": "Checked 24,045 files: 2 damaged copies", "tone": "bad", "complete": false, "finished": true,
           "files": 24045, "good": 24043, "missing": 0, "waiting": 0, "damaged_count": 2,
           "damaged": ["Kate Bush\\Hounds of Love (1985)\\01 Running Up That Hill.flac"],
           "albums": [{"album": "Kate Bush\\Hounds of Love (1985)", "files": ["..."]}]},
 "can_back_up": true, "why_not": null, "busy": false}
```

- `state`: `not_set_up` when neither backup is confirmed ("No backup yet", loud "No backup", line "Pick a drive and Cadence copies your music and its own library there every night."); `needs_look` when any alert is serious ("Your backups need a look", loud "need a look", line the first serious alert); else `good` ("Backed up <how long ago>", the line is the main card's `brief`).
- Alerts are the Music Tool's rules (`alerts_for`): the last backup had a problem, none finished for 2 days, none finished yet, the second backup is 14 days old (gentle). In Cadence the nightly backup is the person's choice, so "No nightly backup is set up." and "turned off in Task Scheduler" are gentle (`serious: false`). Added: the drive's own words when it is the wrong disk or needs a confirm, the main drive away, and a check's damaged copies.
- `drive.state`: `none` (not set up); `away` ("Backup drive is not connected. Nothing can be copied until it is back at F:.", or "Not plugged in" for the second); `wrong_disk` ("The disk at F: is "Pocket", not "Backup drive". Nothing will be copied to it.", or the identity's words when the name matches but the volume does not); `needs_confirm` (the library or the folder is not the confirmed one: the worker's words, such as "The library folder or its drive has changed. Existing backups were left alone. Confirm the folders on the Backup page only if this is the intended library."); `here`.
- `nights`: one per calendar day, oldest first, today last. `done`: a backup that day ended well; `problem`: one ran and had a problem (including a nightly backup that did not start); `missed`: none ran, on a day after the first backup ever; `none`: before the first backup, or today before one has run.
- `by` (on `last`) and `nights_by` (one for each of the 14 nights, beside `nights`) say who started the run: `"nightly"` (the nightly task) or `"hand"` (anything else: Back up now, a mirror, the Music Tool). For a night, it is the run that decided it (the last one that ended well, else the last one); `null` for a missed night or one with no run. A run started by the nightly task writes "Started by the nightly task." under its header in the log, and so does a night that did not start because the library was changing; that line is how it is told. Cadence labels a run outside the schedule "by hand". (The Music Tool's own `--backup-run` writes the same line now.)
- `check.damaged` lists only pairs that still differ (when the drive is here); a pair repaired since is gone from it.
- `can_back_up` false gives `why_not` in words ("Plug in Travel drive to back up to it.", "Choose a drive first.", or the drive's words). `busy`: another backup (the nightly task, the Music Tool) holds this folder now.

`backup.drives` (quick, own thread) `{"folder"}`: the drives a backup could go to, as tiles. Never the library's own drive, never a network drive or a CD.

```json
{"library": {"files": 24045, "bytes": 442381631488},
 "drives": [{"root": "D:\\", "letter": "D:", "name": "Backup drive", "label": "Backup drive", "removable": false,
             "free": 1979120929996, "total": 1999844147200, "fits": true, "folder": "F:\\Backups", "has_backup": true,
             "used_by": "main" | "second" | null, "words": "1.8 TB free · your music needs 412 GB"}]}
```

`folder` is where a backup goes on that drive (`<root>Music Backup`); `has_backup` says a backup is already there (its marker). The library's size is measured once and kept for 2 minutes.

`backup.preview_folders` (quick, own thread) `{"folder", "which", "to": "F:\\Backups"}`: the confirm step's page (picture 10). Only reads; nothing is made or enrolled.

```json
{"which": "main",
 "library": {"path": "X:\\Music", "files": 24045, "bytes": 442381631488},
 "backup": {"path": "F:\\Backups", "exists": false, "has_backup": false, "files": null, "bytes": null,
            "drive": "Backup drive", "letter": "D:", "free": 1979120929996, "total": 1999844147200},
 "to_copy": {"files": 24045, "bytes": 442381631488}, "fits": true, "replaces": null,
 "rules": ["Nothing is copied until you confirm.",
           "Each folder is known by its identity. If either folder is a different one, even at the same drive letter, nothing is copied or moved until you confirm again.",
           "Files are never deleted from the backup. Files that leave your library move to backup history and are kept 30 days."],
 "words": "", "problem": null, "can_confirm": true}
```

`problem` says what stands in the way, in plain words (the folders overlap, the drive itself rather than a folder on it, the second backup inside the main one, a link in the path, the drive not connected, not enough room); `can_confirm` is then false. `replaces` names the folder this backup used before, when it is another one (its copies stay where they are). `words` says when the folder already holds a backup: "This folder already holds a backup. It is kept and brought up to date; nothing in it is deleted."

`backup.confirm_folders` (quick, own thread) `{"folder", "which", "to", "confirm": true}`: enrolls the pair (the worker's `confirm_folders`: both identities bound, the marker made or kept). Copies nothing. Cadence sends it only from the Confirm click; without `"confirm": true` it is `bad_request`. The library must hold files. A new pair clears that backup's "last finished" time. Result `{"confirmed": true, "words": "Library and backup folders confirmed. ..."}`. Refused with `offline_test`, and with `busy` while that backup runs.

`backup.run` (job) `{"folder", "which", "kind": "backup" | "mirror" | "preview"}`.
- `backup` (the default): follows the moves (below), compares the library with the backup by size and date, copies what is new or changed (each read back and compared), keeps replaced copies in `_changed` and moves files that left the library to `_deleted`, saves Cadence's data, clears history older than 30 days, and writes the finish time.
- `mirror`: the same, but files that left the library move to `_deleted` even when there are many (only from a click, after a run answered `too_many_removed`).
- `preview`: what a backup would copy. Writes nothing (not even the log); runs in a test run too.
- Progress, at most every 200 ms and at each stage: `{"stage": "comparing", "words": "Comparing your library with the backup"}`, `{"stage": "following", "words": "Renamed 212 copies to follow moves in the library"}`, `{"stage": "copying", "done": 1204, "total": 2330, "done_bytes": 6549825126, "total_bytes": 12884901888, "words": "Copied and checked 1,204 of 2,330 files, 6.1 GB of 12.0 GB"}`, `{"stage": "saving", "words": "Saving Cadence's library and settings"}`.
- Result of a backup: `{"which", "copied": 2330, "bytes": 12884901888, "new_files", "changed_files", "verified", "kept_old", "removed", "renamed", "up_to_date", "cadence_saved", "new_albums": ["Artist / Album"], "changed_albums": [...], "words": "Backup finished. Copied 2,330 files (12.0 GB).", "brief": "..."}`. `bytes` is exactly what was copied. "Backup finished. Nothing new to copy." when nothing was.
- Result of a preview: `{"which", "to_copy": {"files", "bytes"}, "new_files", "changed_files", "new_albums": [{"name", "detail"}], "changed_albums": [...], "removed", "words": "A backup now would copy 12 files (84 MB)."}`.
- Refused before it starts: `offline_test` (except a preview), `busy` when the engine is changing the library ("Cadence is changing your library right now. Back up again when that is done.") or that backup is already running. Errors from the worker, in its words: `not_set_up`, `not_confirmed`, `wrong_disk`, `drive_away`, `busy` (another backup writes that folder), `too_many_removed`, `no_room`, `cancelled`, `failed`.
- Cancel stops it within one buffer of the file in hand; copies already finished are kept, and nothing half copied takes a file's place.

`backup.check` (job) `{"folder", "which"}`: reads every file in the library and its copy and compares them. Nothing is copied or changed (it writes only its lines in the engine's log, so it runs in a test run too). Cadence asks first, in words (picture 11). Progress `{"stage": "checking", "done": 120, "total": 24045, "words": "Checked 120 of 24,045 files, 1.2 GB of 412 GB"}`. Result `{"which", "files", "good", "missing", "waiting", "damaged_count", "damaged": ["path inside the library"], "albums": [{"album", "files"}], "complete": false, "words": "Checked 24,045 files: 2 damaged copies"}` (`"Checked 24,045 files: every copy matches"` when complete). A finished check is a result even when it found damage; a stopped one is `cancelled`.

`backup.repair_damaged` (job) `{"folder", "which", "choice", "files": ["path inside the library", ...]}`, the files as the check named them (a path with `..` or a drive is `bad_request`):
- `"find_out"`: listens to both files of each pair (`judge_pair`, ffmpeg) and says which is sound. Changes nothing; runs in a test run. Progress per pair `{"done", "total", "file", "sound", "words"}`. Result `{"files": [{"file", "sound": "library" | "copy" | "both" | "neither" | "not_audio" | "gone", "words": "The copy is sound. The library's file is damaged."}]}`. `missing_helper` when a pair is audio and ffmpeg is not there.
- `"copy_again"`: the library's files are sound. Fresh copies are made and checked; the damaged copies are kept in `_changed` for 30 days. Result `{"which", "repaired", "not_repaired": [...], "words"}`.
- `"bring_back"`: the copies are sound. Order: the guard (`Guard::for_library`), the backup's folder checked (`validate_binding`, under its lock, so no backup writes it meanwhile), `library.hold`, then for each file: the folder still there (`gone`), the playing-file wait, read-only and open files left alone and listed, then the copy is brought into the batch's work folder, compared with the copy, and swapped in (`undo::Batch::replaced`). The engine's library lock is held only around each swap, not through the wait, the copy or the listening afterwards, so an import or a repair is never held up for the whole run; a damaged file that changed while its copy was brought out (its size or time) is left as it is now and listed; the damaged file is kept in the batch. One batch of kind `"restore"` ("Backup: brought back 2 files the library had damaged"), so it is in Repair history with Undo. The file that comes in keeps the copy's date, so the next backup copies nothing. Then the report is brought up to date and `library.release`. Nothing in the backup is changed. Result `{"batch", "changed", "problems": ["<file>: why"]}`. Look only does not refuse it (a person asks for it), nor its Undo.
- `copy_again` and `bring_back` are refused with `offline_test`.

`backup.eject` (job) `{"which": "second"}`: asks Windows to let go of the backup's drive (as Eject in File Explorer) and looks whether it is really gone. Result `{"ejected": true, "words": "Travel drive can be pulled out now."}` only once Windows has let go. Otherwise `drive_busy`, with the honest words: "Windows did not let go of the drive: something is still using it. Close what has it open and try again, or eject it from the taskbar. Do not pull it out yet." Also `drive_busy` for a drive that cannot be ejected, one that is not plugged in, or while a backup writes it. Refused with `offline_test`.

`backup.schedule` (quick, own thread) `{"on": true, "folder": "X:\\Music", "at": "03:00"}` or `{"on": false}`. Cadence sends it only from the person's own click, never by itself. On needs the main backup confirmed and here (`not_confirmed` otherwise); `at` is 24-hour `HH:MM` (default `03:00`). Result `{"on": true, "words": "Cadence backs up every night, even when it is closed."}`. Refused with `offline_test`.

### The nightly backup

- **The task**: `Cadence Backup (nightly)` in Task Scheduler, made (or replaced, only if it is Cadence's own) by `backup.schedule` and removed by `on: false`. It runs `cadence-engine.exe --backup-nightly --data "<Cadence's data folder>"` every day at the time chosen (3:00 AM by default), as the signed-in person, with the least rights, at the next sign-in if the PC was off, never on battery limits, at most 12 hours. The engine has no window and no console, so nothing shows. The engine never reads, changes or removes the Music Tool's task `Music Backup (nightly)` (a check when it is built makes the two names differ). A task by Cadence's name that runs something else is left alone (`failed`, in words).
- **Run with Cadence closed**: `--backup-nightly` reads `music_dir` from the engine's settings and backs up the main backup, following the moves, with the same rules as `backup.run`. It writes its run into `logs\backup-log.txt`, so the page shows it. It exits 0 when it finished, 1 when not, 2 without a data folder. A test run (`CADENCE_OFFLINE_TEST`) is refused.
- **Never while the library is changing**: while the engine holds the library (from `library.hold` being given to `library.release`, for every writer: imports, repairs, Undo, Edit details, Organize, bring back), it keeps `<Tools>\library-changing.lock` open with no sharing and "delete on close", so Windows removes it when the engine ends, however it ends. The nightly backup waits up to 15 minutes while that file is held, then does not start, and writes the night into the log as a problem: "Cadence was changing your library, so tonight's backup did not start. Nothing was copied." `backup.run` is refused the same way (`busy`).
- The scheduler is a part (`Parts::scheduler`): the engine's tests use a stand-in that only writes down what it was asked; they never reach Task Scheduler.

### Moves follow

Before a backup copies anything, it applies the moves written down in `<Tools>\backup-moves.jsonl` (Organize, repairs that move files, and the Undo of either; see protocol 3): each copy is renamed to match, oldest batch first, so nothing is copied twice and a moved file never counts as removed.
- The same rules as everything else: only inside the confirmed backup folder (`validate_binding` before each rename), never a reserved name or a link, never over something (a rename with no flags, which never replaces), never a copy. Case-only renames go through a temporary name. Missing folders on the way are made.
- A move is passed over, and the file is then copied as usual, when the library still has something at its old place, when the backup has no copy there, or when something is already where it would go. Nothing is lost either way: whatever the backup does not follow, the comparison that comes next copies again.
- Batches are marked followed per backup, by its marker, in `<Tools>\backup\moves-followed.json` (`{"<marker>": ["batch", ...]}`), once the moves have been looked at, whatever the copying after them did. The main and the second backup each follow every batch, whenever each next runs. A run that stops before them (the wrong disk, folders not confirmed) marks nothing.
- A preview, a check and a repair of copies follow no moves.

### New error codes

| Code | When |
|---|---|
| `not_set_up` | No backup folder has been chosen for that backup. |
| `not_confirmed` | The folders are not the confirmed pair (the library or the backup folder changed, the marker is missing), or the nightly backup was asked for before the main backup was confirmed. |
| `wrong_disk` | Another disk is at the backup's letter (its name, or its volume serial, differs). |
| `drive_away` | The backup's drive is not connected. |
| `too_many_removed` | Many files left the library at once; they were left where they are. `"kind": "mirror"` moves them to backup history. |
| `no_room` | The backup drive has not enough room for what is to be copied. Nothing was copied. |
| `drive_busy` | Windows did not let go of the drive (or it cannot be ejected). |

`busy` also covers a backup already running (here, the nightly task, or the Music Tool) and the library being changed.