# Build และทดสอบ

## Build

ต้องมีแค่ Docker

```bash
build/build.sh                    # ทุกอย่าง และ hub image ที่มีไฟล์ agent อยู่ข้างใน
FLOW_TAG=myhub build/build.sh     # ตั้ง tag ของ image เอง
build/build.sh --no-image         # ไม่ build image
```

build id มาจาก git (`<จำนวน commit>.g<commit>` และต่อท้าย `.dirty.<เวลา>` ถ้ามีไฟล์ที่ยังไม่ commit) หรือตั้งเองด้วย `FLOW_BUILD_ID`

ผลลัพธ์อยู่ใน `build/out/` ซึ่งไม่ถูก commit:

| โฟลเดอร์ | มีอะไร |
| :--- | :--- |
| `linux/` | `flow-agent`, `flow-tray` (glibc 2.35 ขึ้นไป), `.deb`, `install.sh`, `uninstall.sh`, systemd unit, ไฟล์ `.desktop`, ไอคอน |
| `windows/` | `FlowCursor-setup-<เวอร์ชัน>.exe` (NSIS), `flow-agent.exe`, `flow-tray.exe`, `install.ps1`, `uninstall.ps1` |
| `server/` | `flow-server`, `compose.yaml` |
| `hub/agent-builds/` | สิ่งที่ใส่ใน hub image: `manifest.json` และไฟล์ agent ของแต่ละ platform |
| `tools/` | `flow-bench` |

ไอคอนใน `deploy/icons/` วาดด้วยโค้ดเดียวกับไอคอนของ tray สร้างใหม่ด้วย `cargo run -p flow-tray --example gen-icons`

## พัฒนาในเครื่อง

ต้องใช้ Rust stable 1.89 ขึ้นไป (`flow-tray` ต้อง 1.95 ขึ้นไป)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

เช็คโค้ด Windows ใน container ของ build:

```bash
docker build -t flow-cursor-builder -f build/Dockerfile.build build
docker run --rm -v "$PWD":/src flow-cursor-builder cargo clippy --target x86_64-pc-windows-gnu \
  -p flow-agent -p flow-tray -p flow-ipc --all-targets -- -D warnings
```

## Test

`cargo test --workspace` รัน integration test ใน `crates/itest` ด้วย hub จริงและ agent จำลองหลายตัวบน localhost

| ไฟล์ | ทดสอบ |
| :--- | :--- |
| `e2e.rs` | join, ยกเลิกสิทธิ์, สลับ focus, P2P ไป relay และกลับ โดยปุ่มไม่หายไม่ซ้ำ, hub ล่ม |
| `auth.rs` | token ผิด, fingerprint ผิด, การปลอมตัวเป็นเครื่องอื่น |
| `clipboard.rs` | ซิงก์คลิปบอร์ด, จำกัดขนาด, แยกข้อความกับรูป |
| `malformed.rs` | packet เสีย, frame เกินขนาด, datagram ผิดรูป |
| `netgate.rs` | วงที่อนุญาตและปุ่มข้ามจอ |
| `cursor.rs` | ตัวเลือกเคอร์เซอร์และทุกทางที่ควบคุมกลับมา |
| `ondemand.rs` | เปิดและปิด session Remote Desktop ตาม hub, pause, away, ช่วงรอ 10 วินาที, ไม่มีปุ่มค้าง |
| `ipc.rs` | local IPC และสิทธิ์ของ socket |
| `update.rs` | อัปเดตอัตโนมัติ, ไฟล์ที่ hash ไม่ตรง, policy, rollback |

สคริปต์ที่ต้องใช้ Docker หลัง `build/build.sh`:

- `build/e2e-update.sh`: ติดตั้ง build เก่าใน ubuntu:22.04 แล้วดูมันอัปเดตตัวเองจาก hub image จริง และ rollback เมื่อ build ใหม่เสีย
- `build/ui-smoke.sh`: เปิด tray และหน้าต่างสถานะบนจอจำลอง (Xvfb) แล้วบันทึกภาพหน้าจอลง `build/out/ui-smoke/`

## วัด latency

`flow-bench` วัดเวลาจาก capture ถึง inject ที่อีก agent แยก P2P กับ relay

```bash
build/out/tools/flow-bench --samples 1000
```

ตัวอย่างบน localhost (1000 ครั้ง):

| เส้นทาง | event | p50 | p95 | p99 |
| :--- | :--- | ---: | ---: | ---: |
| p2p | key | 97 µs | 139 µs | 160 µs |
| p2p | motion | 152 µs | 243 µs | 312 µs |
| relay | key | 140 µs | 180 µs | 204 µs |
| relay | motion | 199 µs | 293 µs | 370 µs |

ตัวเลขนี้ไม่มี network บน LAN จริงให้ดู RTT ใน dashboard แล้วบวกประมาณครึ่ง RTT ต่อ hop
