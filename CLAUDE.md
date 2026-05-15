# CLAUDE.md — operating notes for this codebase

Lessons learned the hard way while building doremorwater's firmware. Read
before touching anything that runs on the ESP32 task pool.

## Decoding a firmware panic

The release ELF keeps function symbols (`strip = "debuginfo"` in
`Cargo.toml`) so any `Guru Meditation Error` / `Backtrace: …` lines from
serial are translatable to source.

```
Backtrace: 0x4014c90a:0x3ffcc790 0x4014cf1d:0x3ffcc7b0 0x400ef942:…
```

The first hex on each pair is the PC, the second is the SP. Feed the PCs
through `addr2line`:

```sh
A2L=~/.rustup/toolchains/esp/xtensa-esp-elf/esp-*/xtensa-esp-elf/bin/xtensa-esp32-elf-addr2line
ELF=target/firmware/xtensa-esp32-espidf/release/watercontroller-firmware

# one address
$A2L -e $ELF -f -C -p 0x4014c90a

# whole backtrace at once
for a in 0x4014c90a 0x4014cf1d 0x400ef942 0x400f61a3 0x400db036; do
    echo "$a:"; $A2L -e $ELF -f -C -p $a
done
```

Flags: `-f` function names, `-C` demangle Rust, `-p` pretty print.
DWARF is stripped so you only get function names, not line numbers —
fine for diagnosis; the function name is almost always enough.

Verify the ELF you have matches what was running: the panic prints
`ELF file SHA256: <prefix>`; compare with
`sha256sum target/firmware/xtensa-esp32-espidf/release/watercontroller-firmware | head -c 16`.
If they differ you're looking at the wrong binary — rebuild or recover
the image from the device (`j esp32 dump`).

### Register-dump clues without addresses

* `EXCCAUSE: 0x0000001c` = `LoadProhibited`. The CPU read from an
  address that isn't mapped or isn't readable.
* `EXCVADDR` is the offending address.
  * `0x00000000` — straight null deref.
  * `0x00000004`, `0x00000008`, `0x0000000c` — null pointer + small
    offset. Classic Arc/Box/&T deref through a corrupted pointer; the
    offset usually tells you which field of the struct.
  * Addresses in the `0x3fXXXXXX` range that aren't sensible — use-
    after-free or stack-corruption clobbering a live pointer.
* `EXCCAUSE: 0x00000003` = `StoreProhibited`, same idea but writing.
* `EXCCAUSE: 0x00000009` (LoadStoreAlignment) — almost always a wild
  pointer from stack corruption.
* `Stack overflow in task XXX` from FreeRTOS — different mechanism but
  same root cause: a task ran past its allocated stack.

**The panic doesn't always happen at the bug.** Stack-overflow
corruption can clobber a heap pointer in `task A` and only crash later
on `task B` when something dereferences it. The `print_alarm_status`
panic this codebase hit was triggered three `println!` calls earlier
on a *different* task; the panic surfaced one wifi-probe tick later.
If the backtrace is in unrelated code, look at which other tasks have
tight stacks (`/api/diag` or the `tasks` serial CLI command).

### Find tight tasks

```sh
curl -s http://<device-ip>/api/diag | jq '.tasks | sort_by(.stack_min_free_bytes)'
```

or, over serial:

```
> tasks
```

Anything under ~500 B free is suspicious. Anything under ~200 B is the
likely culprit when a stack-overflow panic shows up.

## Stack discipline on the ESP32

We run on FreeRTOS with small pthread stacks. Several tasks live close
to their ceiling because adding margin everywhere wastes ~6 KiB per
task on a chip that has ~280 KiB of internal DRAM. Stay aware:

| Task          | Stack | Typical HWM free |
|---------------|------:|-----------------:|
| `wifi-sup`    | 16 KB | ~420 B           |
| `serial-cli`  |  8 KB | ~200–400 B       |
| `httpd`       | 13 KB | ~11 KB           |
| `mqtt-sup`    |  6 KB | a few hundred B  |

If a HWM is under 200 B you're one cosmic ray from a panic. The fixes
that have worked here, in order of preference:

### Rules

1. **One dynamic format argument per `println!` / `log::*!`.**
   `core::fmt` allocates ~700 B of stack per `{}` placeholder for the
   `&dyn Display` + `Argument` + `Formatter` machinery. A 5-arg line
   peaks at ~3.5 KB. On `wifi-sup` and `serial-cli` that's enough to
   crater the task.

   ```rust
   // BAD — five dynamic args, ~3.5 KB transient stack
   log::info!("flow {flow:.1} | thr {thr} | dur {dur} | el {el} | act {act}");

   // GOOD — same content, ~700 B each, returns between calls
   log::info!("flow alarm: {state}");
   log::info!("  threshold {thr} L/h");
   log::info!("  duration  {dur} s");
   log::info!("  elapsed   {el} s");
   ```

   If you absolutely need multi-arg, hoist the format into a
   `#[inline(never)]` helper so the locals live only while that
   function is on the stack, not as part of the caller's permanent
   frame.

2. **Don't clone the `Config` to read it.** `App::config()` returns
   `Arc<Config>`. A clone of the inner struct is 6–10 KB (HTTPS PEMs,
   MQTT TLS, schedule rules, …). Pass `Arc<Config>` around and deref;
   only call `(*app.config()).clone()` at the call site that needs to
   mutate before `replace_config`.

   ```rust
   // BAD — clones the whole Config onto the caller's stack frame
   let cfg = app.config();             // before Arc refactor: Config
   if cfg.flow_alarm.enabled { … }

   // GOOD (today) — Arc<Config>, ~16 B stack
   let cfg = app.config();
   if cfg.flow_alarm.enabled { … }
   ```

3. **`{:?}` on a real struct is expensive.** Debug-derive emits a deep
   format chain that recurses field by field. Use targeted `{}`
   placeholders instead. The wifi-event-subscription crash early in
   this repo was a `{:?}` on a multi-field struct from an event-loop
   callback — the closure ran on the wifi-sup task and ate its stack.

4. **Heavy work goes in `#[inline(never)] fn`s.** Rust's prologue
   reserves space for *all* locals across *every* match arm or branch
   at function entry, even paths that rarely run. A function like
   `evaluate_flow_alarm` containing a `log::error!` in the rare-fire
   branch will burn that stack on every tick unless the heavy branch
   is split out. The wifi supervisor's `run()` is the canonical
   example — each phase (`run_connected`, `run_ap_fallback`,
   `enter_ap_mode`, …) is its own `#[inline(never)]` helper.

5. **Closures captured by event callbacks run on the *subscribing*
   task.** `EspSystemEventLoop::subscribe(|event| …)` looks innocuous;
   it actually invokes the closure on whichever task posts the event.
   For WIFI events that's often the supervisor itself. Anything you
   do inside the closure adds to the supervisor's stack budget. Keep
   them trivial — drop the data into an `AtomicU8`/`Mutex` and act on
   it from the supervisor's normal loop.

6. **Hold `&Arc<T>` over the lifetime of a borrow.** Don't move
   string fields out of a refcounted Config:

   ```rust
   // BAD — moves String out of Arc<Config>; doesn't compile, and
   // even with .clone() wastes a heap String per call
   let token = app.config().admin_token;

   // GOOD — keep the Arc alive across the read
   let cfg = app.config();
   let header_matches = req.header("Authorization")
       .and_then(|h| h.strip_prefix("Bearer "))
       == Some(cfg.admin_token.as_str());
   ```

7. **Watch for closures that bring large captures into long-lived
   tasks.** If you `move ||` a struct that contains a 4 KB cert into
   a spawned pthread, that's 4 KB consumed from that task's stack on
   spawn. Prefer `Arc<…>` and move the Arc.

### When you must add stack

If you've split, removed `{:?}`, factored out `#[inline(never)]` and
the HWM is still too tight: bump the task's `spawn_named` size in
512-byte steps and re-measure via `/api/diag` after each
representative run. Don't double the stack blindly — that often
hides a real issue and costs more DRAM than the fix would.

## Hardware testing loop

```
jmp shell -l target=esp32
$ make device-test           # build, one-shot flash, run pytest
$ make device-test WC_NO_RESET=1 WC_DEVICE_IP=192.168.1.182
                             # iterate on test code only, skip reset
```

`tests/playwright/conftest.py` opens a session-scoped `PexpectAdapter`
on `client.serial` and sets `logfile_read = sys.__stderr__.buffer`.
Every UART byte hits stderr — visible live with `pytest -s`, captured
in pytest's "Captured stderr" section of failure reports otherwise.
This is how you see a panic backtrace from a test that "just timed
out". Don't run a separate `j serial pipe` alongside the suite; the
lab gRPC layer holds the FTDI flock exclusively and you'll deadlock.

After-test diag prints `wifi-sup`, `serial-cli`, `httpd`, `sys_evt`
HWMs in the terminal. Watch them across the run — a drift downward
across consecutive tests means you broke discipline somewhere.

## Other gotchas

* **PSRAM threshold**: `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL=16384`.
  Anything bigger than that lands on PSRAM (32-bit access only). DMA
  buffers, anything that needs 8-bit access, must stay in DRAM —
  allocate with `MALLOC_CAP_INTERNAL`.

* **OTA selector**: serial-flashing `ota_0` doesn't change `otadata`,
  so a device that previously OTA'd into `ota_1` keeps booting the
  stale `ota_1` image. `make device-test` wipes `otadata` (0xff x
  0x2000 at offset `0xf000`) before flashing `ota_0`. If you flash
  manually, do the same.

* **NVS persistence across sessions**: NVS at `0x9000` size `0x6000`
  retains config (sprinkler auto-off, alarm threshold, OTA'd certs)
  across reboots and OTA. `make device-test` also wipes NVS so each
  session starts from compile-time defaults + the build-time WiFi
  seed from `.env`. Skip the wipe at your peril — stale config breaks
  tests that assume defaults.

* **HWM history doesn't reset across `replace_config` calls.** It's
  cumulative since the last boot. To compare HWMs across configs you
  need a fresh boot in between.

* **`/api/diag` is unauthenticated** but cheap — poll it freely.
  `/api/config/*` PUTs are authenticated when `admin_token` is set
  (see `require_auth` in `firmware/src/http_server.rs`).

* **`make ota IP=<addr>`** for fast iteration on a connected device:
  ~15 s wall time vs ~25 s for full serial flash. Doesn't wipe
  otadata/NVS — use serial flash for clean-slate tests.

* **OTA's final reboot can fail under internal-DRAM pressure.** The
  upload itself doesn't allocate much, but the post-upload
  `ota-reboot` task (2 KiB stack, spawned via `task_util::spawn_named`)
  needs a contiguous chunk of internal DRAM. Under a TLS handshake
  storm or a fragmented heap you'll see `spawn_named "ota-reboot"
  failed: Not enough space (os error 12)` and `Failed to create task!`
  on serial; the new firmware is in flash but unbooted. Workaround:
  send `reset` over the serial CLI (or power-cycle). Root cause is
  whatever fragmented internal DRAM in the first place; see "TLS
  handshake storm" mitigations in `http_server.rs`.

* **`pthread_mutex_unlock` panic in `try_connect_sta` (task #40).**
  The crash is inside `esp_idf_svc::wifi::BlockingWifi`'s cond_var
  lifecycle when we run the stop→set_config→start→connect cycle
  rapidly under a DHCP-failing AP. The proper fix is dropping
  `BlockingWifi` and driving `EspWifi` ourselves with our own event
  handling; that's a large refactor and currently deferred because
  a static-IP DHCP reservation eliminates the trigger.
