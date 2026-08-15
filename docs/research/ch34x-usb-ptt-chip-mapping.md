# CH34x raw-USB PTT: status-register bit mapping, polarity, and chip detection

**Status:** findings from hardware-in-the-loop testing on a real AllScan UCI150
(2026-06-23), plus authoritative cross-checks against the Linux `ch341.c` driver
and the WCH CH343 datasheet.

**Why this exists:** the raw-USB PTT backend (`crates/astar-ptt/src/uci150_usb.rs`,
nuggets `iax-ceba`/`iax-d937`) read the modem-status lines using the **CH341**
register bit map. That map is **wrong for the CH343** in the UCI150, so a request
for `CTS` silently returned the `DCD` line. This documents the correct,
chip-specific behavior and how to detect which chip you're talking to. Tracked
as `iax-f56c` (this repo) and `astar-fc4e` (astar).

---

## 0. Sandbox constraint (governs everything below)

The target is a **sandboxed / Mac App Store** build, so the **only** device
access is **raw USB via nusb** (IOKit USBHost + the `com.apple.security.device.usb`
entitlement). No `/dev/cu.*` tty, no `ioctl`s, no sysfs, no reliance on the WCH
driver. Therefore **all** detection and I/O here use only:

- **USB descriptors** — `vendor_id`, `product_id`, `bcdDevice` (device version),
  and interface `class` (all from `nusb::list_devices()` / the device
  descriptor); and
- **control transfers** on the default control pipe (`nusb` `control_in` /
  `control_out`), which work without claiming an interface.

Every detection signal and read path in this document is nusb-compatible by
construction — none of it needs the tty or the kernel driver.

## 1. The transport (recap)

astar's serial use is **pure PTT signalling** — read an operator-key input line,
drive a radio-key output line. The backend does this over the USB **device
default control pipe**, with no `/dev/cu.*` tty and without claiming any
interface (it coexists with macOS's built-in CDC-ACM driver; only the WCH *dext*
must be absent):

- **Read modem status:** Vendor control IN — `bRequest = 0x95`, `wValue = 0x0706`,
  `wLength = 2`; take `byte[0]` = the modem-status byte.
- **Drive RTS/DTR:** CDC class control OUT — `bRequest = 0x22`
  (`SET_CONTROL_LINE_STATE`), `wValue` bitmap `bit0=DTR, bit1=RTS`,
  `wIndex = comm-interface`.

## 2. Polarity — always active-low on the wire, active-high in logic

The CH343 datasheet states every MODEM **input** (CTS, DSR, RI, DCD) is
**active-low** at the pin. So in the status byte a line is **asserted when its
bit reads 0**; idle reads `0xff`. The backend inverts this to a logical
"keyed = true" — i.e. the decoded result is **active-high, always**. There is
**no polarity option**: decode active-low → return active-high. (This also
matches ASL3's own UCI path, `carrierfrom = usbinvert`.)

## 3. The bug: the status-byte bit map is chip-specific

### CH341 / CH340 — authoritative (Linux `ch341.c`)

```c
#define CH341_BIT_CTS 0x01   // bit0
#define CH341_BIT_DSR 0x02   // bit1
#define CH341_BIT_RI  0x04   // bit2
#define CH341_BIT_DCD 0x08   // bit3
// status = ~data & 0x0f;  (active-low)  bit0->CTS bit1->DSR bit2->RI bit3->DCD
```

This is the map the spike copied. Correct for CH340/CH341.

### CH343 (UCI150) — empirical, and **reversed** for CTS/DCD

The CH343 datasheet is **pin-level only** — WCH does **not** publish the legacy
`0x95` register byte order for this chip. (The CH343 natively enumerates as
CDC-ACM, whose standard `SERIAL_STATE` notification reports DCD/DSR/RI but
**not CTS** — so the vendor register is the *only* way to read CTS, and its bit
order is undocumented.) The mapping below comes from bench measurement,
cross-referenced against the WCH driver's known-correct line labels:

| `MicPTT Dest` switch (WCH driver reports…) | idle | keyed | bit that moved | ⇒ on CH343 that bit is |
|---|---|---|---|---|
| **CTS** | `0xff` | `0xf7` | **bit3** | **CTS** |
| **DCD** | `0xff` | `0xfe` | **bit0** | **DCD** |

```
0xff = 1111 1111  (idle)
0xf7 = 1111 0111  (CTS-position keyed → bit3 low)
0xfe = 1111 1110  (DCD-position keyed → bit0 low)
```

So on the **CH343**: **`bit3 = CTS`, `bit0 = DCD`** — the opposite of the CH341.
`CTS`/`DCD` are confirmed (they're the PTT lines). **`DSR`/`RI` (bit1/bit2) are
unconfirmed** — most likely a full nibble reversal (`bit2 = DSR, bit1 = RI`), but
that is not proven by any document or test.

### Root cause: it's the *register*, not the *chip generation*

The reversal is **not** a CH341-vs-CH343 pin/map difference; it is a difference
between two status **sources on the same CH343**:

- the **interrupt-IN notification** — what WCH's own driver and macOS's CDC-ACM
  driver decode (`ch343.c` `CH343_CTI_C=0x01 … CH343_CTI_DC=0x08`, i.e. the
  canonical `CTS=bit0 … DCD=bit3`); and
- the legacy **`0x95` status register** — which returns that nibble
  **bit-reversed**, and which WCH's CH343 driver **never reads for status**.

This reconciles both observations: testing through the kernel CH34x driver
(notification path) shows CTS/DCD labelled correctly, while the raw-USB backend
(`0x95` path) sees them swapped. Re-confirmed 2026-06-23 with `--monitor` against
the `0x95` register: "CTS" switch position keyed → `0xf7` (bit3), "DCD" position
keyed → `0xfe` (bit0).

**Why we can't just use the notification path:** it lives on an interrupt-IN
endpoint that requires claiming the interface, and on macOS the **built-in
CDC-ACM driver holds that interface even with no third-party dext installed**
(claim fails `0xe00002c5` / `kIOReturnExclusiveAccess`). A sandboxed/MAS app
cannot detach a kernel driver, so the `0x95` control-pipe read is the **only**
viable transport — and therefore the backend must decode the reversed `0x95`
map directly.

## 4. Chip detection

The USB **vendor id is always `0x1A86`**; discriminate by **product id**, with
the **protocol generation** (device class) as a custom-PID-proof corroborator:

| PID | chip | enumerates as | use map |
|---|---|---|---|
| `0x7523` | CH340 | vendor-specific (class `0xff`) | **CH341 map** |
| `0x5523` | CH341 | vendor-specific (class `0xff`) | **CH341 map** |
| `0x55D3` | **CH343** | CDC-ACM (comm `0x02` + data `0x0a`) | **CH343 map** |
| `0x55D2` | CH9102/CH343-family | CDC-ACM | CH343 map *(verify)* |
| `0x55D4` | CH9102 | CDC-ACM | CH343 map *(verify)* |
| `0x55D5` | CH9103 | CDC-ACM | CH343 map *(verify)* |
| `0x55D8` | CH9101 | CDC-ACM | CH343 map *(verify)* |

- **Primary:** PID lookup (table above). Reliable for stock devices; the UCI150
  is `0x55D3` = CH343.
- **Corroborator (handles custom VID/PID):** the protocol generation correlates
  with the bit-map difference. **Vendor-class** (an interface with class `0xff`,
  no CDC comm interface) ⇒ CH340/CH341 ⇒ CH341 map. **CDC-ACM** (a class-`0x02`
  comm interface) ⇒ CH343 generation ⇒ CH343 map.
- **Optional, finer:** the chip-version vendor request (`ch341.c`'s
  `CH341_REQ_READ_VERSION = 0x5F`) returns a version byte that further
  distinguishes variants.

## 5. Recommended implementation

**Principle: the hardware is deterministically detectable, so detection is
required. Failing to identify the device is a bug, not a fallback condition —
surface it; never silently guess a map.**

1. **Detect the chip — mandatory.** Identify it over nusb (descriptors only):
   `product_id` against the table in §4, corroborated by the protocol generation
   (CDC-ACM vs vendor class). Resolve to a known chip and its `bit → line` map. A
   request for `KeyLine::Cts` then reads the physically-correct bit (`bit3` on a
   CH343, `bit0` on a CH341), and the UI label is truthful.
2. **Unidentifiable device ⇒ explicit error.** If the device is a WCH serial chip
   we don't have a mapping for (unknown PID *and* no class match), return a clear
   error (e.g. `PttError::Unsupported("unrecognized WCH chip 0x….; needs a
   verified bit map")`). Do **not** apply a default/guessed map — a wrong map
   silently miskeys the radio, which is worse than a hard failure.
3. **Polarity:** always decode active-low → active-high logical. No toggle.

> A "press PTT and watch which bit moves" routine is fine as an *optional
> diagnostic / dev tool* (it's what `--monitor` already is) for verifying a
> chip's map or onboarding a new chip — but it is **not** a substitute for
> detection in the shipping product.

## 6. Open items

- Confirm the **CH343 DSR/RI** bits (bit1/bit2) — assumed reversed, unverified.
- Confirm whether **CH9102/CH9103/CH9101** share the CH343 map.
- A definitive register-order source would be a WCH-internal doc or the WCH
  driver source; absent that, the calibration approach (§5.2) is the safe path.

## 7. References

- `crates/astar-ptt/src/uci150_usb.rs` — the backend (CH343 0x95 map; fixed in iax-f56c).
- `crates/astar-serial-sys/examples/ch34x_usb_spike.rs` — the de-risk spike +
  `--monitor` diagnostic (prints the live raw status byte).
- Linux `ch341.c` — authoritative CH341 bit definitions.
- WCH CH343 datasheet v1E — active-low MODEM pins (no register byte order).
- Nuggets: `iax-ceba`, `iax-d937` (backend); `iax-f56c` (this fix); astar `astar-fc4e`.
