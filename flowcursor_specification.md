# FlowCursor (`flow-cursor`)

> **Centralized Software KVM Switch** สำหรับแชร์ Mouse, Keyboard และ Clipboard ข้ามเครื่องภายใน Local Network โดยมี Hub บน HomeLab ทำหน้าที่เป็น **Control Plane** (ทะเบียนเครื่อง, Layout, Focus, Settings, Clipboard, Pairing) ส่วน Input วิ่ง **ตรงระหว่างเครื่อง (P2P)** และใช้ Hub เป็นทางสำรอง (Relay)

> เอกสารนี้ปรับปรุงให้ตรงกับสิ่งที่สร้างจริงในเวอร์ชันแรก (MVP) หลังการหารือออกแบบกับเจ้าของโปรเจกต์ — ข้อตัดสินใจในหัวข้อ 2 มีผลเหนือแนวคิดเดิม รายละเอียดการติดตั้ง/ใช้งานอยู่ใน `README.md`

---

## 1. Executive Summary & Goals

### 1.1 แนวคิดของระบบ
โปรแกรมตระกูล Software KVM (Mouse without Borders, Barrier, Input Leap) มักทำงานแบบ **Host-Client** ผูก Mouse/Keyboard กับเครื่องหลักเครื่องเดียว ถ้าเครื่องนั้นดับหรือหลับ ระบบทั้งหมดหยุด

**FlowCursor** ออกแบบสำหรับ **HomeLab / Local Server First**:
* **Hub (`flow-server`):** รันเป็น Docker container บน Home Server (host network) เก็บ Screen Layout, Focus state, Settings, Clipboard broker, ทะเบียนเครื่อง + Pairing, สมุดที่อยู่ peer และ Web dashboard — และเป็น **Relay สำรอง** สำหรับ input
* **Agent (`flow-agent`):** ติดตั้งบนทุกเครื่อง (**Linux** Wayland/X11 และ **Windows 11**, x86_64) ทำทั้ง **Capture** และ **Inject** — ไม่มี macOS

### 1.2 จุดประสงค์หลัก (Core Objectives)
1. **ทุกเครื่องเป็นต้นทางได้ (Every machine is an input source):** แตะอุปกรณ์ของเครื่องไหนก็คุมเครื่องอื่นได้ ไม่มีเครื่องหลัก เครื่องใดออฟไลน์ไม่กระทบเครื่องอื่น (Hub ล่ม: P2P ยังใช้ layout/peer ที่ cache ไว้ทำงานต่อได้)
2. **Ultra-Low Latency:** เป้าหมาย < 5 ms บน LAN — วัดบน localhost ได้ p50 ≈ 0.1 ms (key, P2P) / ≈ 0.14 ms (relay) ด้วย `flow-bench`
3. **Cross-Platform:** Windows 11 และ Linux (GNOME Wayland ≥ 45 ผ่าน portal, Xorg ผ่าน X11)
4. **Seamless Edge Detection:** ข้ามขอบจอแบบ proportional แม้ความละเอียด/จำนวนจอต่างกัน
5. **ปลอดภัยโดยปริยาย:** ทุกการเชื่อมต่อเข้ารหัส (QUIC/TLS 1.3) และยืนยันตัวตนทั้งสองฝั่ง ไม่มีข้อมูลคีย์บอร์ด plaintext บนสาย

---

## 2. System Architecture

```
                    +-------------------------------------------+
                    |   flow-server (Hub, Docker host network)  |
                    |  - Node registry + pairing (join token)   |
                    |  - Screen layout (persisted)  - Focus     |
                    |  - Settings (push live)  - Clipboard      |
                    |  - Peer address book + fingerprints       |
                    |  - Input RELAY (fallback)  - Dashboard    |
                    |  - mDNS _flowcursor._udp                  |
                    +---------+-----------------------+---------+
               QUIC UDP 7878  |                       |  QUIC UDP 7878
          (control / relay / clipboard / datagrams)   |
                    +---------+---------+   +---------+---------+
                    | flow-agent (Linux)|<->| flow-agent (Win11)|
                    | InputCapture/libei|P2P| LL hooks/RawInput |
                    | RemoteDesktop/X11 |UDP| SendInput (scan)  |
                    +-------------------+7879+-------------------+
```

### 2.1 ข้อตัดสินใจหลัก (Fixed decisions)
1. **ทุกเครื่องเป็น input source** — เครื่องต้นทางตัดสินการข้ามขอบเองจาก layout (cache ไว้) แล้วแจ้ง Hub (`FocusChanged`) Hub เก็บตาราง focus (source → target) และแสดงใน dashboard
2. **Control plane vs Data plane** — Hub = control plane; input ส่ง **agent → agent ตรง (P2P)** โดยปริยาย **สลับไป relay ผ่าน Hub อัตโนมัติ** เมื่อเส้นตรงล้ม และ **กลับ P2P อัตโนมัติ** เมื่อ probe สำเร็จ โดยไม่ทำให้ event หายหรือซ้ำ
3. **Transport: QUIC (`quinn`) ทั้งหมด** — reliable stream สำหรับ control, key/button, clipboard; **QUIC datagram** สำหรับ mouse motion และ ping
4. **Keycode กลาง = USB HID usage** (`page << 16 | id`; หน้า 0x07 + modifier, ปุ่มมีเดียหน้า 0x0C) ตารางแปลง Linux evdev และ Windows **scancode** (E0 = extended) — ไม่ใช้ VK
5. **Safety:** release-all ทุกครั้งที่สลับ focus / ตัดการเชื่อมต่อ / สลับเส้นทาง, heartbeat timeout, **emergency hotkey** (ค่าเริ่มต้น Ctrl+Alt+Shift+Esc, ตั้งค่าได้)
6. **ภาษา Rust ทั้งหมด** ใน Cargo workspace เดียว

### 2.2 Communication Channels (ทั้งหมดอยู่ใน QUIC, TLS 1.3)
* **Agent → Hub (ALPN `flowcursor-hub/1`, UDP 7878)** — agent เปิดทุก stream:
  * *Control stream:* `ToHub`/`FromHub` (Hello/Welcome, ping, monitors, focus, path reports, layout, peers, settings, focus table, release, revoke)
  * *Relay stream:* `RelayUp{to, LinkMsg}` / `RelayDown{from, LinkMsg}` — Hub ใส่ `from` จากตัวตนที่ยืนยันแล้วเสมอ
  * *Clipboard stream:* แยกออกมาเพื่อไม่ให้รูปภาพก้อนใหญ่ขวาง input
  * *Datagrams:* `RelayDatagramUp/Down` (motion, ping/pong ผ่าน relay)
* **Agent → Agent (ALPN `flowcursor-p2p/1`, UDP 7879)** — ผู้ส่ง (source) เป็นฝ่ายเปิดการเชื่อมต่อ:
  * stream เดียวของ `LinkMsg` (Event/Ack) + datagram `LinkDatagram` (Motion, Ping, Pong)

### 2.3 Path manager (P2P ↔ Relay)
* แต่ละคู่ source→target มี *link actor* (`crates/net/src/link/sender.rs`)
* Reliable event มี `(session, seq, base)` ฝั่งส่งเก็บจนได้ `Ack` ฝั่งรับ (`receiver.rs`) apply ทีละ seq ครั้งเดียวตามลำดับ (มี reorder buffer)
* **ประกาศเส้นตรงล้ม** เมื่อ: connection ปิด, มี event ไม่ได้ ack นานกว่า `fail_timeout` (700 ms), หรือไม่มี pong นานกว่า max(700 ms, 3 × ping interval)
* เมื่อสลับเส้นทาง: ส่งซ้ำทุก event ที่ยังไม่ ack ทางเส้นใหม่ แล้วส่ง `StateSync{keys, buttons}` ให้ฝั่งรับปล่อยปุ่มที่ไม่ตรง
* ขณะใช้ relay: probe เส้นตรงทุก `probe_interval` (ตั้งได้, ค่าเริ่มต้น 2 s) pong ติดกัน 2 ครั้ง → กลับ P2P
* Motion: datagram มีตำแหน่ง absolute + `after_seq` (ไม่ apply ก่อน event reliable ที่ส่งก่อนหน้า — ถ้ามาถึงก่อนจะถูกพักไว้แล้ว apply ทันทีที่ event นั้นมาถึง)
* ฝั่งรับปล่อยทุกปุ่มของ source ที่เงียบเกิน 2.5 s; ฝั่งส่งคืนการควบคุมเองเมื่อไม่มีเส้นทางเกิน 1.5 s
* นโยบาย `prefer_p2p` / `force_relay` และ path + RTT ของทุกคู่ แสดงใน log และ dashboard

### 2.4 ความปลอดภัย (Authentication & Pairing)
* **ไม่มี CA** — ทุกฝ่ายใช้ self-signed cert (ECDSA P-256) ตัวตน = SHA-256 fingerprint ของ cert
* **Hub identity** ถาวรใน volume (`/data/hub.crt`, `hub.key`)
* **Agent identity** สร้างในเครื่อง private key ไม่เคยออกจากเครื่อง (ไฟล์สิทธิ์ 0600)
* **Join token** `FC1-<secret 16 ไบต์>-<16 ไบต์แรกของ fingerprint Hub>` (base32): agent ยืนยัน Hub *ก่อน* ส่ง secret, Hub เทียบ secret แบบ constant-time และหน่วง 500 ms เมื่อผิด, token regenerate ได้ใน dashboard
* หลัง join agent pin fingerprint เต็มของ Hub; Hub บันทึก fingerprint ของ agent เป็น credential ของ node
* **P2P:** mutual TLS — ผู้โทรคาดหวัง fingerprint ของปลายทางตรงตัว ผู้รับยอมเฉพาะ fingerprint ที่ Hub แจก; revoke → ทุก agent ปิดการเชื่อมต่อของเครื่องนั้น และ agent ที่ถูก revoke จะไม่ join ซ้ำเองจาก token ใน config
* Dashboard ป้องกันด้วย bearer token (`FLOW_ADMIN_TOKEN` หรือ token สุ่มที่เก็บใน volume)

---

## 3. Technology Stack (ที่ใช้จริง)

| ส่วนประกอบ | เครื่องมือ | เหตุผล |
| :--- | :--- | :--- |
| ภาษา | **Rust** (workspace เดียว) | ประสิทธิภาพสูง, binary เดียวต่อเครื่อง, cross-compile ไป Windows ได้ |
| Transport | **QUIC** (`quinn` + `rustls`/ring) | เข้ารหัสในตัว, stream + datagram ในการเชื่อมต่อเดียว, ไม่มี head-of-line ระหว่าง stream |
| Serialization | **postcard** (serde, varint) | กะทัดรัดเทียบ protobuf (key event < 24 ไบต์, motion < 20 ไบต์), ไม่ต้องมี codegen/`protoc`, ชนิดเดียวกันใช้ทำ JSON ให้ dashboard ได้; ความเข้ากันได้ข้ามเวอร์ชันใช้ `PROTOCOL_VERSION` ตรวจตอน Hello แทน field evolution ของ protobuf |
| Hub web | `axum`, HTML/JS ฝังใน binary | ไม่มีขั้นตอน build ฝั่งเว็บ |
| Discovery | `mdns-sd` | pure Rust, ประกาศ `_flowcursor._udp.local.` |
| Linux Wayland | `ashpd` (xdg-desktop-portal: InputCapture, RemoteDesktop, Clipboard) + `reis` (libei) | วิธีมาตรฐานบน GNOME ≥ 45 (แบบเดียวกับ Input Leap) ไม่ต้องใช้ C library |
| Linux X11 | `x11rb` (XInput2, XTEST, XFixes, RandR) | ไม่ต้องอยู่กลุ่ม `input` หรือ udev rule (ใช้แทน evdev/uinput) |
| Windows | `windows-sys` (`WH_*_LL`, Raw Input, `SendInput`, clipboard) | API ดั้งเดิม ตอบสนองไว |

---

## 4. Work Breakdown & Implementation Phases (สถานะ MVP)

### Phase 1: Core Agent Input Primitive
- [x] **Windows:** `WH_MOUSE_LL`/`WH_KEYBOARD_LL` (กลืน input เฉพาะตอนคุมเครื่องอื่น, ไม่จับ event ที่ถูก inject), Raw Input สำหรับ delta, `SendInput` ด้วย scancode, ตรวจขอบจากตำแหน่งที่ไม่ถูก clip ของ hook
- [x] **Linux Wayland:** InputCapture portal (pointer barrier ตาม layout) + libei receiver; RemoteDesktop portal + libei sender (fallback D-Bus `Notify*`); เก็บ restore token
- [x] **Linux X11:** XInput2 raw motion + grab, XTEST inject
- [x] **Simulated backend** สำหรับทดสอบอัตโนมัติ
- [ ] ทดสอบบนฮาร์ดแวร์จริง (เจ้าของโปรเจกต์จะทดสอบ)

### Phase 2: Central Hub & Network Protocol
- [x] Message schema (`proto/`): Hello/Welcome, Layout, Peers, Settings, Focus, PathReport, Relay, LinkMsg/LinkDatagram, Clipboard
- [x] Registration + pairing ด้วย join token, revoke credential
- [x] Layout engine (`crates/layout`): หลายจอ/ต่างความละเอียด, proportional mapping, ขอบที่นำไปเครื่องอื่น, virtual cursor
- [x] Focus tracking ที่ Hub, Relay สำรอง, P2P ↔ relay failover/recovery
- [x] Settings เก็บที่ Hub และ push สดไปทุก agent

### Phase 3: Seamless Transition & Edge Detection
- [x] Edge behaviour: switch delay, double bump, dead corners
- [x] เลื่อนต่อข้ามหลายเครื่อง และกลับเครื่องตัวเองพร้อม warp pointer ไปจุดที่สอดคล้อง
- [x] Keyboard redirection ไปเครื่องที่ focus อยู่ + release-all ทุกครั้งที่สลับ
- [x] Emergency hotkey (ค่าเริ่มต้น Ctrl+Alt+Shift+Esc)

### Phase 4: Shared Features & Polish
- [x] **Clipboard sync** ข้อความ + รูป (PNG เป็นรูปแบบกลาง), เปิด/ปิด/จำกัดขนาดแยกชนิด
  - GNOME: Clipboard portal บน RemoteDesktop session (GNOME ≥ 45) → สำรองด้วย X11 selection ผ่าน XWayland
  - Windows: CF_UNICODETEXT, "PNG", CF_DIB/CF_DIBV5
- [x] **Web Dashboard:** สถานะเครื่อง, path/RTT ต่อคู่, ลากจัด layout, join token, หน้า Settings
- [x] **Auto-Discovery:** mDNS (ระบุที่อยู่ Hub เองใน config ได้เสมอ)
- [x] Build ผ่าน Docker (`build/build.sh`): Linux binary + `.deb` + installer + systemd user unit, Windows `.exe` + PowerShell installer, Hub image
- [x] Latency bench (`flow-bench`)
- [ ] Windows SYSTEM service สำหรับ UAC / lock screen (นอกขอบเขต MVP)

---

## 5. Event Packet Structure (ที่ใช้จริง, `proto/src/lib.rs`)

```rust
/// Reliable (QUIC stream), ทั้ง P2P และใน RelayUp/RelayDown
enum LinkMsg {
    /// base = seq ที่เก่าที่สุดที่ผู้ส่งยังเก็บไว้ (ใช้เมื่อผู้รับยังไม่รู้จัก session)
    Event { session: u64, seq: u64, base: u64, ev: InputEvent },
    Ack { session: u64, upto: u64 },
}

enum InputEvent {
    Enter { x: i32, y: i32 },                 // พิกัดในเครื่องปลายทาง
    Leave,                                     // ผู้รับปล่อยทุกปุ่ม
    Key { usage: u32, down: bool },            // HID usage (page << 16 | id)
    Button { button: Button, down: bool },     // Left/Right/Middle/Back/Forward
    Wheel { dx: i32, dy: i32 },                // หน่วย 1/120 notch
    StateSync { keys: Vec<u32>, buttons: Vec<Button> },
}

/// Unreliable (QUIC datagram)
enum LinkDatagram {
    Motion(Motion),                // { session, mseq, after_seq, x, y, dx, dy }
    Ping { session: u64, nonce: u64 },
    Pong { nonce: u64 },
}
```

ทุกข้อความ encode ด้วย postcard และส่งเป็น frame `u32 LE length + payload` (control ≤ 1 MiB, clipboard ≤ 64 MiB) — ข้อความเสีย/ตัดท้าย/มีไบต์เกินถือเป็น error ไม่ panic

---

## 6. Next Steps Checklist
1. เจ้าของโปรเจกต์ deploy Hub บน Home Server (`deploy/compose.yaml`) และติดตั้ง agent บน Zorin OS และ Windows 11 (`build/out/`)
2. ตรวจเวอร์ชัน GNOME บน Zorin: InputCapture ต้องการ GNOME ≥ 45 — ถ้าเป็น Zorin OS 17 (ฐาน Ubuntu 22.04) ให้ใช้ session Xorg หรืออัปเกรดเป็น Zorin OS 18
3. วัด latency จริงบน LAN (RTT ใน dashboard, `flow-bench` เป็นค่าอ้างอิง localhost) และปรับ mouse speed ต่อเครื่อง
4. ทดสอบ failover จริง (เช่นบล็อก UDP 7879 ด้วย firewall) และดูว่าสลับ relay/กลับ P2P ตาม log
5. งานถัดไป: Windows SYSTEM service (UAC/lock screen), InputCapture v2 + restore token เมื่อ GNOME รองรับ, HTTPS สำหรับ dashboard, tray icon
