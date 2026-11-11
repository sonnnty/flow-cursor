# FlowCursor (`flow-cursor`)

**Software KVM** สำหรับแชร์เมาส์ คีย์บอร์ด และคลิปบอร์ด (ข้อความ + รูปภาพ) ระหว่างคอมพิวเตอร์ในวง LAN เดียวกัน
มี **Hub** (`flow-server`) รันเป็น Docker container บน Home Server และ **Agent** (`flow-agent`) บนทุกเครื่อง
รองรับตอนนี้: **Linux x86_64** (Zorin OS / Ubuntu, GNOME บน Wayland หรือ Xorg) และ **Windows 11 x86_64**

> สถานะ: เวอร์ชันแรก (MVP) — ทดสอบอัตโนมัติด้วย backend จำลองครบทุกเส้นทาง (ลงทะเบียน, สลับ focus, P2P ↔ relay, คลิปบอร์ด)
> แต่ **ยังไม่ได้ทดสอบกับฮาร์ดแวร์จริง** (Wayland/GNOME และ Windows) ดูหัวข้อ [ข้อจำกัดที่รู้อยู่](#ข้อจำกัดที่รู้อยู่)

---

## สารบัญ
1. [หลักการทำงาน](#หลักการทำงาน)
2. [สถาปัตยกรรม](#สถาปัตยกรรม)
3. [ติดตั้ง Hub ด้วย Docker Compose](#ติดตั้ง-hub-ด้วย-docker-compose)
4. [ติดตั้ง Agent บน Zorin OS / Linux](#ติดตั้ง-agent-บน-zorin-os--linux)
5. [ติดตั้ง Agent บน Windows 11](#ติดตั้ง-agent-บน-windows-11)
6. [Dashboard และหน้า Settings](#dashboard-และหน้า-settings)
7. [ไฟล์ตั้งค่าของ Agent](#ไฟล์ตั้งค่าของ-agent)
8. [Emergency hotkey](#emergency-hotkey)
9. [Build จากซอร์ส](#build-จากซอร์ส)
10. [การทดสอบและวัด latency](#การทดสอบและวัด-latency)
11. [ข้อจำกัดที่รู้อยู่](#ข้อจำกัดที่รู้อยู่)
12. [แก้ปัญหาเบื้องต้น](#แก้ปัญหาเบื้องต้น)

---

## หลักการทำงาน

* **ทุกเครื่องเป็นต้นทางของ input ได้** — แตะเมาส์/คีย์บอร์ดของเครื่องไหน ก็ใช้คุมเครื่องอื่นได้ ไม่มี "เครื่องหลัก"
  เครื่องใดดับหรือหลับ เครื่องอื่นยังทำงานต่อได้
* เมื่อเลื่อนเมาส์ชนขอบจอที่ layout ระบุว่าต่อกับอีกเครื่อง agent ของเครื่องต้นทางจะ "จับ" (capture) เมาส์/คีย์บอร์ดของตัวเอง
  แล้วส่ง event ไปให้เครื่องปลายทาง inject — เลื่อนต่อไปเครื่องที่สามได้ และเลื่อนกลับก็คืนการควบคุมให้เครื่องตัวเอง
* ตำแหน่งที่โผล่ฝั่งปลายทางคำนวณแบบ **proportional** (ชนขอบที่ 50% ความสูง → โผล่ที่ 50% ของอีกเครื่อง แม้ความละเอียดต่างกัน)
* คลิปบอร์ด (ข้อความ / รูป) ที่ก็อปบนเครื่องใด จะถูกส่งไปทุกเครื่องผ่าน Hub (เปิด/ปิด และจำกัดขนาดได้ใน Settings)

## สถาปัตยกรรม

```
                    ┌───────────────────────────────────────────┐
                    │   flow-server (Hub, Docker, host network) │
                    │  - ทะเบียนเครื่อง + pairing (join token)    │
                    │  - Layout จอ (บันทึกลง volume)             │
                    │  - Focus state, Settings, Clipboard broker │
                    │  - สมุดที่อยู่ peer + fingerprint            │
                    │  - Relay input (สำรองเมื่อ P2P ใช้ไม่ได้)     │
                    │  - Web dashboard :7880, mDNS               │
                    └──────┬──────────────────────────┬─────────┘
          QUIC :7878 (control, relay, clipboard)   QUIC :7878
                           │                          │
                 ┌─────────┴────────┐  QUIC P2P  ┌────┴─────────────┐
                 │ flow-agent (Linux)│◄──:7879──►│ flow-agent (Win11)│
                 │ capture/inject    │  input     │ capture/inject    │
                 └───────────────────┘            └───────────────────┘
```

**Control plane vs data plane**
* Hub = control plane: ทะเบียน, layout, focus, settings, clipboard, สมุดที่อยู่, dashboard
* Input = data plane: ส่ง **agent → agent ตรง (P2P)** เป็นค่าเริ่มต้น ถ้าเส้นตรงใช้ไม่ได้จะ **สลับไป relay ผ่าน Hub อัตโนมัติ**
  และ probe เส้นตรงทุก `probe_interval` เมื่อกลับมาใช้ได้ (pong ติดกัน 2 ครั้ง) ก็ **สลับกลับ P2P อัตโนมัติ**
* ระหว่างสลับเส้นทาง event ของปุ่ม/คีย์ **ไม่หายและไม่ซ้ำ**: ทุก event มี `(session, seq)` ฝั่งส่งเก็บไว้จนได้ ack
  และส่งซ้ำทั้งหมดที่ยังไม่ ack เมื่อเปลี่ยนเส้นทาง ฝั่งรับ apply แต่ละ seq ครั้งเดียวตามลำดับ หลังสลับยังส่ง `StateSync`
  (ชุดปุ่มที่ฝั่งส่งเชื่อว่ากดค้างอยู่) ให้ฝั่งรับปล่อยปุ่มที่ไม่ตรงกัน — กันปุ่มค้าง
* เมาส์เคลื่อนที่ส่งเป็น QUIC datagram (unreliable) พร้อมตำแหน่ง absolute ทำให้ datagram ที่หายไม่มีผล
* path ปัจจุบัน (p2p/relay) และ RTT ของทุกคู่เครื่อง แสดงใน log และใน dashboard

**Transport และความปลอดภัย** — ทุกอย่างวิ่งบน QUIC (`quinn`, TLS 1.3) ไม่มีข้อมูลคีย์บอร์ดเป็น plaintext บนสาย
* Hub มี identity ถาวร (self-signed cert เก็บใน volume `/data`)
* Agent แต่ละเครื่องสร้าง key pair + cert ของตัวเอง (private key ไม่ออกจากเครื่อง) — fingerprint (SHA-256 ของ cert) คือ credential
* **Join token** = `FC1-<secret>-<16 ไบต์แรกของ fingerprint ของ Hub>` ทำให้ agent ตรวจสอบตัวตน Hub **ก่อน** ส่ง secret
  (ไม่มีช่วง trust-on-first-use) หลังเข้าร่วมแล้ว agent pin fingerprint เต็มของ Hub ไว้
* Hub รับ client cert ใดก็ได้ในระดับ TLS แล้วตัดสินในระดับแอป: cert ที่ลงทะเบียนแล้ว → node เดิม,
  cert ใหม่ → ต้องมี join token ที่ถูกต้อง (เทียบแบบ constant-time, หน่วง 500 ms เมื่อผิด)
* การเชื่อม P2P เป็น mutual TLS: ผู้โทรคาดหวัง fingerprint ของเครื่องปลายทางตรงตัว ผู้รับยอมเฉพาะ fingerprint ที่ Hub แจกมา
  ลบ/revoke เครื่องใน dashboard → ทุก agent ตัดการเชื่อมต่อของเครื่องนั้นทันที
* Agent ปลอมตัวเป็นเครื่องอื่นไม่ได้ เพราะตัวตนผูกกับ private key ของ cert และฝั่งรับใช้ตัวตนจากการเชื่อมต่อ ไม่ใช่จากข้อความ
* โหมด relay: เข้ารหัสทีละช่วง (agent↔hub, hub↔agent) — Hub เป็นผู้ที่ไว้ใจได้ จึงเห็น event ภายใน process ของตัวเอง

**Keycode กลาง** = USB HID usage (`page << 16 | id`, หน้า 0x07 + modifier; ปุ่มมีเดียในหน้า 0x0C)
แปลงไป/กลับ Linux evdev `KEY_*` และ Windows **scancode** (E0 = extended) ไม่ใช้ VK code จึงไม่ขึ้นกับ keyboard layout

**โครงสร้าง repo** (Cargo workspace เดียว ภาษา Rust ทั้งหมด)

| ไดเรกทอรี | หน้าที่ |
| :--- | :--- |
| `proto/` | ชนิดข้อความทั้งหมด (encode ด้วย `postcard`) |
| `crates/keymap` | ตาราง HID ↔ evdev ↔ Windows scancode, ชื่อปุ่ม/hotkey |
| `crates/layout` | layout engine: เพื่อนบ้าน, edge crossing แบบ proportional, virtual cursor |
| `crates/net` | identity/cert, QUIC + pinning, framing, join token, mDNS, **link/path manager** (P2P ↔ relay) |
| `server/` | `flow-server` (Hub) + dashboard (HTML ฝังใน binary) |
| `agent/` | `flow-agent` + backend: `wayland`, `x11`, `windows`, `sim` (จำลอง) |
| `crates/itest` | integration test (hub + agent จำลองหลายตัว) |
| `tools/bench` | `flow-bench` วัด latency P2P vs relay |
| `build/`, `deploy/` | สคริปต์ build ผ่าน Docker, Dockerfile, compose, installer, systemd unit |

## ติดตั้ง Hub ด้วย Docker Compose

ต้องใช้ `network_mode: host` (QUIC/UDP และ mDNS multicast ข้าม bridge network ของ Docker ไม่ได้)

```bash
git clone <repo> flow-cursor && cd flow-cursor/deploy
FLOW_ADMIN_TOKEN='ตั้งรหัส-dashboard-ยาวๆ' docker compose up -d --build
docker compose logs flow-server | grep -E "join token|dashboard token|fingerprint"
```

* พอร์ต: **UDP 7878** (agent ↔ hub, QUIC), **TCP 7880** (dashboard), UDP 5353 (mDNS) — เปิด firewall ของ server ด้วย
* ข้อมูลทั้งหมดอยู่ใน volume `flowcursor-data` (`/data`): `hub.crt`/`hub.key` (identity), `state.json` (เครื่อง, layout, settings, secret)
  **อย่าลบ volume** ไม่เช่นนั้น fingerprint ของ Hub จะเปลี่ยนและทุก agent ต้อง join ใหม่
* ถ้าไม่ตั้ง `FLOW_ADMIN_TOKEN` Hub จะสุ่ม token, เก็บใน volume และพิมพ์ไว้ใน log
* ใช้ image ที่ build ไว้แล้ว: `build/build.sh` สร้าง `flow-cursor-server:flowcursor-mvp` (ดู [Build](#build-จากซอร์ส))
* ตัวแปรเพิ่มเติม: `FLOW_QUIC_ADDR`, `FLOW_HTTP_ADDR`, `FLOW_NO_MDNS=1`, `FLOW_NAME`, `RUST_LOG`
* พิมพ์ join token (ระหว่างที่ hub รันอยู่ก็ได้): `docker compose exec flow-server flow-server --print-token` — image เป็น distroless จึงไม่มี shell

เปิด dashboard ที่ `http://<server>:7880` แล้วใส่ dashboard token

## ติดตั้ง Agent บน Zorin OS / Linux

### 0) ตรวจเวอร์ชัน GNOME ก่อน (สำคัญ)
การจับเมาส์บน **Wayland** ใช้ portal **InputCapture** ซึ่งมีใน **GNOME ≥ 45** (xdg-desktop-portal ≥ 1.18) เท่านั้น

```bash
echo $XDG_SESSION_TYPE                 # wayland หรือ x11
gnome-shell --version
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop | grep -E "InputCapture|RemoteDesktop|Clipboard"
```

* เห็น `org.freedesktop.portal.InputCapture` → ใช้ Wayland ได้เต็มรูปแบบ (เช่น Ubuntu 24.04, Zorin OS 18 ที่ใช้ GNOME 46)
* **ไม่เห็น InputCapture** (เท่าที่ตรวจสอบได้ Zorin OS 17 อิง Ubuntu 22.04 และใช้ GNOME Shell ตระกูล 43) →
  เครื่องนี้ยัง *ถูกควบคุม* ได้ผ่าน RemoteDesktop portal แต่ *ไปควบคุมเครื่องอื่น* บน Wayland ไม่ได้
  ให้ logout แล้วเลือก session **"Zorin Desktop on Xorg"** (รูปเฟืองที่หน้า login) — backend `x11` ทำงานครบทั้ง capture/inject/clipboard
  โดยไม่ต้องใช้สิทธิ์พิเศษ

### 1) ติดตั้ง
จาก `build/out/linux/` (ผลจาก `build/build.sh`):

```bash
# แบบไม่ใช้ root (แนะนำ): ติดตั้งลง ~/.local/bin + systemd user service
./install.sh FC1-XXXX-YYYY            # หา hub ด้วย mDNS
./install.sh FC1-XXXX-YYYY 192.168.1.10   # หรือระบุที่อยู่ hub

# หรือ .deb
sudo apt install ./flow-agent_0.1.0_amd64.deb
flow-agent join FC1-XXXX-YYYY --hub 192.168.1.10
systemctl --user enable --now flow-agent
```

ดูสถานะ/ log: `systemctl --user status flow-agent`, `journalctl --user -u flow-agent -f`, `flow-agent status`

### 2) หน้าต่างขออนุญาตของ GNOME (Wayland)
ครั้งแรกที่ agent เริ่ม GNOME จะถาม:
1. **Remote Desktop / "Allow remote interaction"** — ให้เครื่องอื่นควบคุมเครื่องนี้ (และใช้คลิปบอร์ดผ่าน portal)
   agent ขอแบบ `persist_mode = explicitly revoked` และเก็บ **restore token** ไว้ที่ `~/.local/share/flowcursor/portal-tokens.json`
   ครั้งต่อไปจะไม่ถามอีก (ถ้าอยากให้ถามใหม่: `flow-agent forget-portal` แล้ว restart service)
2. **Input Capture** — ให้เครื่องนี้ไปควบคุมเครื่องอื่น InputCapture v1 ของ GNOME 46 ไม่มี restore token
   จึงอาจถามทุกครั้งที่ agent เริ่ม/เมื่อ layout ของขอบจอเปลี่ยน (GNOME 46 เปิดใช้ session เดิมซ้ำไม่ได้ ต้องสร้าง session ใหม่)

### 3) คลิปบอร์ดบน GNOME
GNOME ไม่รองรับ `wlr-data-control` โปรแกรมเบื้องหลังจึงอ่าน/เขียนคลิปบอร์ด Wayland ตรงๆ ไม่ได้ agent จึงใช้ตามลำดับ:
1. **Clipboard portal** ผูกกับ RemoteDesktop session (xdg-desktop-portal ≥ 1.17, GNOME ≥ 45) — วิธีหลัก ได้ทั้งข้อความและ PNG
2. ถ้าไม่มี: **X11 CLIPBOARD selection ผ่าน XWayland** (mutter sync คลิปบอร์ด Wayland ↔ X11 ให้) — ต้องมี XWayland ทำงานอยู่
   (บน GNOME ที่เปิด XWayland แบบ on-demand อาจยังไม่ทำงานจนกว่าจะมีโปรแกรม X11 เปิดอยู่)
3. Xorg session: ใช้ X11 selection ตรงๆ

### 4) ทางเลือก backend
`backend = "auto"` (ค่าเริ่มต้น) เลือก `wayland` เมื่อ `XDG_SESSION_TYPE=wayland` มิฉะนั้น `x11`
บังคับได้ใน config หรือ `flow-agent run --backend x11` (backend evdev/uinput ไม่ได้ทำ เพราะ X11 API ทำงานได้โดยไม่ต้องอยู่กลุ่ม `input`/udev rule)

## ติดตั้ง Agent บน Windows 11

จาก `build/out/windows/` เปิด PowerShell ในโฟลเดอร์นั้น:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Token FC1-XXXX-YYYY -Hub 192.168.1.10
```

* ติดตั้งที่ `%LOCALAPPDATA%\Programs\FlowCursor\flow-agent.exe` และตั้งให้เริ่มตอน login (registry `HKCU\...\Run`) ไม่ต้องใช้ admin
* ครั้งแรก Windows Firewall อาจถาม — อนุญาตบน **Private network** (agent รับ P2P ที่ UDP 7879) ถ้ารัน installer เป็น admin จะสร้าง rule ให้
* `-Elevated` (ต้องเปิด PowerShell แบบ admin): ให้ agent รันแบบ elevated ผ่าน Scheduled Task ทำให้ควบคุมหน้าต่างของโปรแกรมที่รันแบบ admin ได้
* Log: `%LOCALAPPDATA%\flowcursor\data\agent.log` · config: `%APPDATA%\flowcursor\config\agent.toml`
* ถอนการติดตั้ง: `.\uninstall.ps1` (เพิ่ม `-Purge` เพื่อลบ identity/ข้อมูลด้วย)
* **ข้อจำกัด:** หน้าต่าง UAC, หน้าจอล็อก และ Ctrl+Alt+Del อยู่บน secure desktop ซึ่งโปรแกรมระดับผู้ใช้ควบคุมไม่ได้
  ต้องมี Windows service ที่รันเป็น SYSTEM (ยังไม่ได้ทำในเวอร์ชันนี้)

## Dashboard และหน้า Settings

`http://<server>:7880` (ป้องกันด้วย dashboard token, ส่งเป็น `Authorization: Bearer`)

* **Overview**: join token (copy / regenerate), ลากจัดตำแหน่งเครื่อง (ขอบจะ snap ติดกัน) แล้วกด *Save layout*,
  ตารางเครื่อง (สถานะ online, OS, จอ, กำลังคุมเครื่องไหน, **mouse speed ต่อเครื่อง**, เปิด/ปิด, **Remove = revoke credential**),
  ตาราง **path (p2p/relay) และ RTT** ของทุกคู่ และคลิปบอร์ดล่าสุด (เฉพาะชนิด/ขนาด ไม่แสดงเนื้อหา)
* **Settings** (บันทึกใน volume และ push ไปทุก agent ทันที ไม่ต้อง restart):
  emergency hotkey, เปิด/ปิด clipboard sync และแยก text/image + ขนาดสูงสุด,
  พฤติกรรมขอบจอ (switch delay, double bump, dead corners), input path (prefer P2P / force relay), probe interval, log level
* เปลี่ยนชื่อ/ปิด/ลบเครื่อง และ regenerate join token อยู่ในหน้า Overview
* Dashboard เป็น HTTP ธรรมดา — ใช้ใน LAN ที่ไว้ใจได้ หรือวาง reverse proxy ที่มี TLS ไว้ด้านหน้า

## ไฟล์ตั้งค่าของ Agent

ตำแหน่ง: Linux `~/.config/flowcursor/agent.toml`, Windows `%APPDATA%\flowcursor\config\agent.toml`
(ระบุไฟล์อื่นได้ด้วย `--config` หรือ `FLOW_AGENT_CONFIG`) ทุกค่าเป็นทางเลือก

```toml
hub = "192.168.1.10"          # host หรือ host:port (ไม่ใส่ = หาผ่าน mDNS)
# hub_fingerprint = "6866f7…" # pin fingerprint ของ hub เอง (ปกติได้จาก token อัตโนมัติ)
# join_token = "FC1-…"        # สำหรับติดตั้งแบบอัตโนมัติ (ปกติใช้ `flow-agent join`)
p2p_port = 7879               # UDP สำหรับ P2P (0 = สุ่ม)
# p2p_advertise = ["192.168.1.20:7879"]   # ที่อยู่ที่เครื่องอื่นใช้ติดต่อเครื่องนี้ (กรณี NAT/หลาย NIC)
backend = "auto"              # auto | wayland | x11 | windows | sim
# emergency_hotkey = "Ctrl+Alt+Shift+Escape"  # override ในเครื่อง ใช้ได้แม้ hub ล่ม
# log_level = "info"          # override log level จาก hub
# log_file = "/tmp/flow-agent.log"
# state_dir = "/path"         # ที่เก็บ identity/สถานะ (ค่าเริ่มต้น ~/.local/share/flowcursor)
```

ไฟล์ที่ agent เขียนเอง (ใน `state_dir`, สิทธิ์ 0600): `agent.crt`/`agent.key` (identity), `agent-state.json`
(node id, fingerprint ของ hub ที่ pin ไว้, ที่อยู่ hub ล่าสุด), `portal-tokens.json` (Linux)

คำสั่ง: `flow-agent run | join <token> [--hub] | status | forget-portal | monitors`

ค่าที่ต้องทำงานได้แม้ไม่มี hub (ที่อยู่ hub, fingerprint, hotkey) อยู่ในไฟล์นี้ ค่าอื่นมาจาก Settings ของ hub
ถ้า hub ล่ม agent ยังใช้ layout/สมุดที่อยู่ล่าสุดที่ cache ไว้และคุมกันผ่าน P2P ต่อได้ (relay ใช้ไม่ได้จนกว่า hub กลับมา)

## Emergency hotkey

ค่าเริ่มต้น **Ctrl + Alt + Shift + Esc** — กดบนคีย์บอร์ดของเครื่องที่กำลังคุมเครื่องอื่นอยู่ จะคืนการควบคุมให้เครื่องตัวเองทันที
(ส่ง key-up ของทุกปุ่มที่กดค้างไปยังเครื่องปลายทางก่อนเสมอ) เปลี่ยนได้ใน Settings หรือ override ใน `agent.toml`
รูปแบบ: ชื่อปุ่มคั่นด้วย `+` เช่น `Ctrl+Alt+F12`, `LeftCtrl+RightCtrl` (`Ctrl`/`Shift`/`Alt`/`Super` = ซ้ายหรือขวาก็ได้) ต้องมีอย่างน้อย 2 ปุ่ม

กลไกความปลอดภัยอื่น: release-all ทุกครั้งที่สลับ focus / ตัดการเชื่อมต่อ / สลับเส้นทาง (ผ่าน StateSync),
ฝั่งที่ถูกคุมปล่อยทุกปุ่มเองถ้าไม่ได้ยินอะไรจากต้นทาง 2.5 วินาที, ฝั่งต้นทางคืนการควบคุมเองถ้าไม่มีเส้นทางไปปลายทาง 1.5 วินาที

## Build จากซอร์ส

ต้องมีแค่ Docker:

```bash
build/build.sh            # ทุกอย่าง + hub image flow-cursor-server:flowcursor-mvp
build/build.sh --no-image # ไม่ build image
```

ผลลัพธ์ใน `build/out/` (ไม่ commit):
`linux/` (`flow-agent` glibc ≥ 2.35, `.deb`, `install.sh`, `uninstall.sh`, `flow-agent.service`),
`windows/` (`flow-agent.exe` cross-compile ด้วย mingw, `install.ps1`, `uninstall.ps1`),
`server/` (`flow-server`, `compose.yaml`), `tools/flow-bench`, `SHA256SUMS`

พัฒนาด้วย Rust ในเครื่อง (stable ≥ 1.88):
```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
# ตรวจว่าโค้ด Windows compile ได้ (ต้องมี mingw สำหรับ ring — หรือใช้ container ของ build/)
docker build -t flow-cursor-builder -f build/Dockerfile.build build
docker run --rm -v "$PWD":/src flow-cursor-builder cargo clippy --target x86_64-pc-windows-gnu --workspace -- -D warnings
```

## การทดสอบและวัด latency

* `cargo test --workspace` รวม integration test ใน `crates/itest` ที่รัน Hub จริง + agent จำลอง 3 เครื่องบน localhost ผ่าน QUIC จริง:
  ลงทะเบียนด้วย token, ปฏิเสธ token ผิด / token ของ hub อื่น, revoke, สลับ focus A→B→C→A, input ไปถึงเครื่องที่ถูกต้องเท่านั้น,
  release-all เมื่อสลับ, emergency hotkey, **P2P เป็นค่าเริ่มต้น → ปิดกั้นเส้นตรง (UDP proxy) → relay โดย event ไม่หาย/ไม่ซ้ำ → กลับ P2P**,
  clipboard ข้อความ/รูป, settings push สด, P2P ทำงานขณะ hub ล่ม, watchdog ปล่อยปุ่มค้าง
* `flow-bench` (`build/out/tools/flow-bench` หรือ `cargo run --release -p flow-bench`) วัด latency ปลายถึงปลาย
  (event จาก capture → inject ที่อีก agent) บน localhost แยก P2P / relay เช่น (เครื่อง build นี้, 1000 ตัวอย่าง):

  | path | event | p50 | p95 | p99 |
  | :--- | :--- | ---: | ---: | ---: |
  | p2p | key | 97 µs | 139 µs | 160 µs |
  | p2p | motion | 152 µs | 243 µs | 312 µs |
  | relay | key | 140 µs | 180 µs | 204 µs |
  | relay | motion | 199 µs | 293 µs | 370 µs |

  เป็นตัวเลข localhost (ไม่มี network) — บน LAN จริงดู RTT ใน dashboard แล้วบวกประมาณครึ่ง RTT ต่อ hop

## ข้อจำกัดที่รู้อยู่

* **ยังไม่ได้ทดสอบบนเครื่องจริง** — backend Wayland (portal + libei), X11 และ Windows compile ผ่านและตรรกะหลักถูกทดสอบด้วย backend จำลอง
  แต่การเรียก OS จริงอาจต้องปรับหลังทดสอบบนเครื่อง
* Wayland ต้องการ GNOME ≥ 45 สำหรับการ *ไปควบคุม* เครื่องอื่น (InputCapture); เก่ากว่านั้นใช้ Xorg session
* GNOME 46: InputCapture อาจถามสิทธิ์ทุกครั้งที่เริ่ม agent หรือเมื่อ layout เปลี่ยน (ไม่มี restore token ใน v1, และ re-enable session ไม่ได้)
* ระหว่างถูกคุมผ่าน portal ขณะที่ไม่มีอุปกรณ์ absolute pointer ใน EIS จะใช้การเลื่อนแบบ relative (อาจคลาดเล็กน้อยกับ pointer acceleration)
* Windows: ใช้ raw mouse delta (ไม่มี acceleration ของ Windows) — ปรับ **mouse speed ต่อเครื่อง** ใน dashboard ให้รู้สึกเหมือนเดิม
* Windows: ควบคุม UAC / lock screen / secure desktop ไม่ได้ (ต้องมี SYSTEM service); หน้าต่าง elevated ต้องติดตั้งแบบ `-Elevated`
* Clipboard: ส่งแบบ eager ไปทุกเครื่องทันทีที่ก็อป (ภายในขนาดที่ตั้ง), รูปภาพแลกเปลี่ยนเป็น PNG; X11 ฝั่งที่เป็นเจ้าของ selection
  ส่งข้อมูลก้อนเดียว (ไม่ใช้ INCR) จำกัดตามขนาด request สูงสุดของ X server (~16 MiB)
* Dashboard ใช้ HTTP ธรรมดา; Hub เห็น input ที่ relay ภายใน process (เข้ารหัสทีละช่วง ไม่ใช่ end-to-end)
* Hub และ agent ต้องเป็นเวอร์ชัน protocol เดียวกัน (ตรวจตอนเชื่อมต่อ ถ้าไม่ตรงจะแจ้งให้อัปเดต)
* ไม่รองรับ macOS (ตั้งใจ)

## แก้ปัญหาเบื้องต้น

| อาการ | ตรวจ |
| :--- | :--- |
| agent ต่อ hub ไม่ได้: `invalid peer certificate` | fingerprint ไม่ตรง — hub ถูกสร้างใหม่ (volume หาย) หรือ token ผิด hub → `flow-agent join <token ใหม่>` |
| `hub rejected this agent: invalid join token` | token ถูก regenerate แล้ว — ใช้ token ล่าสุดจาก dashboard |
| `this agent was removed from the hub` | ถูก revoke ใน dashboard — ต้อง `flow-agent join` ใหม่ |
| path เป็น `relay` ตลอด | firewall บล็อก UDP 7879 ระหว่างเครื่อง หรือที่อยู่ที่ประกาศผิด (ตั้ง `p2p_advertise`) |
| เมาส์ไม่ข้ามจอ | layout ใน dashboard ต้องให้ขอบเครื่องชิดกัน, เครื่องปลายทางต้อง online และ enabled; Wayland ต้องอนุญาต Input Capture |
| ปุ่มค้าง | กด emergency hotkey; ฝั่งที่ถูกคุมจะปล่อยเองภายใน 2.5 วินาทีถ้าต้นทางหายไป |
| GNOME ไม่ถามสิทธิ์อีก / อยากรีเซ็ต | `flow-agent forget-portal` แล้ว `systemctl --user restart flow-agent` |
| log ละเอียด | Settings → log level = `debug` (หรือ `flow_net=trace`) |
