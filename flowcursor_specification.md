# FlowCursor (`flow-cursor`)

> **Centralized Software KVM Switch** สำหรับแชร์ Mouse, Keyboard และ Clipboard ข้ามเครื่อง (Cross-Platform) ภายในระบบเครือข่าย Local Network โดยมี Server บน HomeLab ทำหน้าที่เป็นตัวควบคุมหลัก (Orchestrator / State Coordinator)

---

## 1. Executive Summary & Goals

### 1.1 แนวคิดของระบบ
โดยทั่วไป โปรแกรมตระกูล Software KVM (เช่น Mouse without Borders, Barrier, Input Leap) จะทำงานแบบ **Host-Client (P2P)** ซึ่งต้องผูก Mouse/Keyboard เข้ากับเครื่องหลักเครื่องใดเครื่องหนึ่ง หากเครื่องนั้นดับหรือหลับ (Sleep) ระบบทั้งหมดจะหยุดทำงาน

**FlowCursor** ออกแบบมาสำหรับสภาพแวดล้อม **HomeLab / Local Server First**:
* **Central Hub (`flow-server`):** รันเป็น Service/Container อยู่บน Home Server ทำหน้าที่เก็บ Screen Topology (แผนผังหน้าจอ), จัดการ State การสลับเครื่อง, Authentication, และทำหน้าที่เป็น Router/Signaling สำหรับ Input Events
* **Node Agent (`flow-agent`):** ติดตั้งบนคอมพิวเตอร์ลูกข่ายแต่ละเครื่อง (Windows, Linux X11/Wayland, macOS) ทำหน้าที่ทั้ง **ดักจับ (Capture)** และ **จำลอง (Inject)** เมาส์และคีย์บอร์ด

### 1.2 จุดประสงค์หลัก (Core Objectives)
1. **Always-Available Topology:** ปิดหรือเปิดคอมพิวเตอร์เครื่องใดเครื่องหนึ่งได้อิสระ โดยไม่ทำให้โครงข่ายการเคลื่อนเมาส์ของเครื่องอื่นพัง
2. **Ultra-Low Latency:** เวลาแฝงในการส่งพิกัดเมาส์ต้องน้อยกว่า 5ms บน Local Network
3. **Cross-Platform:** รองรับทั้ง Windows 10/11 และ Linux (ครอบคลุมทั้ง X11 และ Wayland)
4. **Seamless Edge Detection:** เคลื่อนเมาส์ทะลุขอบหน้าจอเครื่องหนึ่งไปยังอีกเครื่องหนึ่งได้อย่างเป็นธรรมชาติ

---

## 2. System Architecture

```
                      +-----------------------------+
                      |   FlowCursor Hub (Server)   |
                      |   - Topology & Layout Map   |
                      |   - Active Focus Controller  |
                      |   - Shared Clipboard Broker |
                      +--------------+--------------+
                                     |
               +---------------------+---------------------+
               | TCP (State/Control) | UDP (Stream Events) |
               v                                           v
    +-----------------------+                   +-----------------------+
    |   Node A (Windows)    |                   |    Node B (Linux)     |
    |  `flow-agent`         |                   |   `flow-agent`        |
    |  - Hooks: Win32 API   |                   |  - Hooks: X11 / evdev |
    |  - Inject: SendInput  |                   |  - Inject: uinput/libei|
    +-----------------------+                   +-----------------------+
```

### 2.1 Communication Channels
* **Control Channel (TCP / WebSocket):** 
  * ใช้สำหรับ Register เครื่องเข้าระบบ, ส่ง Heartbeat, อัปเดต Layout การวางจอ, และซิงก์ Clipboard Data
* **Input Event Stream (UDP or Raw TCP via Protobuf):**
  * สำหรับส่งพิกัดเมาส์ (`dx`, `dy`), สถานะปุ่มกด (`MouseButtonEvent`), และคีย์บอร์ด (`KeyEvent`) เพื่อลด Overhead ให้เหลือน้อยที่สุด

---

## 3. Technology Stack Selection (แนะนำ)

| ส่วนประกอบ | ภาษา / เครื่องมือที่แนะนำ | เหตุผล |
| :--- | :--- | :--- |
| **Agent Core** | **Rust** หรือ **Go** | ประสิทธิภาพสูง คุม Memory ได้ดี มี C-bindings สำหรับ OS API ครบ |
| **Server Hub** | **Go** หรือ **Rust** | รันเบา เหมาะกับ Container/Docker บน HomeLab รองรับ Concurrency สูง |
| **Data Serialization**| **Protocol Buffers** หรือ **MessagePack** | Payload ขนาดเล็ก ถอดรหัสไว ประหยัดแบนด์วิดท์ |
| **Windows Hooks** | Win32 API (`SetWindowsHookEx`, `SendInput`) | มาตรฐานดั้งเดิม ตอบสนองไว |
| **Linux Hooks** | `evdev` (Read), `uinput` หรือ `libei` (Inject) | รองรับได้ทั้ง X11 และ Wayland ยุคใหม่ |

---

## 4. Work Breakdown & Implementation Phases

### Phase 1: Core Agent Input Primitive (OS-level Input)
- [ ] **Windows Implementation:**
  - สร้างโมดูลดักจับ Low-level Mouse & Keyboard (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`)
  - สร้างโมดูลจำลองการกด (`SendInput`)
  - ฟังก์ชัน "Lock Cursor" ไว้ที่ขอบจอเพื่อเตรียมกระโดดข้ามเครื่อง
- [ ] **Linux Implementation:**
  - จัดการสิทธิ์การอ่าน Input ผ่าน `/dev/input/event*` (`evdev`)
  - สร้าง Virtual Device ผ่าน `/dev/uinput` สำหรับจำลอง Input
  - ตรวจสอบการใช้งานบน Wayland ผ่านโปรโตคอล `libei`

### Phase 2: Central Hub & Network Protocol
- [ ] **Protocol Design:**
  - กำหนด Message Schema (NodeRegister, ScreenLayoutUpdate, CursorPosition, KeyEvent, ClipboardSync)
- [ ] **`flow-server` Development:**
  - ระบบรับ Registration ของแต่ละ Agent (ID, Resolution, Screen Position)
  - Layout Engine: คำนวณ Boundary Mapping (เช่น จอ Node-A อยู่ซ้าย, Node-B อยู่ขวา)
  - Focus Switch Logic: เมื่อ Node-A ส่ง Cursor ออกที่ $X = \text{Width}$, ส่งคำสั่ง Activate ให้ Node-B รับ Focus ต่อทันที

### Phase 3: Seamless Transition & Edge Detection
- [ ] การจัดการเมาส์ติดขอบ (Border Trapping):
  - ดักจับเมื่อ Pointer ชนขอบหน้าจอ
  - ส่งพิกัดความเร็ว (Velocity) หรือจุดที่ชนไปยัง Server
  - ปล่อย Pointer บนเครื่องต้นทาง แล้วสร้าง Virtual Pointer โผล่ที่ขอบของเครื่องปลายทาง
- [ ] Keyboard Redirection:
  - เมื่อ Focus อยู่ที่เครื่องไหน ให้ส่ง Event แป้นพิมพ์ไปยังเครื่องนั้นโดยอัตโนมัติ

### Phase 4: Shared Features & Polish
- [ ] **Clipboard Synchronization:** รองรับการก็อปปี้ Plain Text และ Images ระหว่างเครื่อง
- [ ] **Web Dashboard / TUI บน Server:** หน้าเว็บสำหรับลากจัดตำแหน่งหน้าจอ (Layout Configurator)
- [ ] **Auto-Discovery:** ใช้ mDNS (Bonjour/Avahi) ให้ Agent ค้นหา HomeLab Server ในวง LAN ได้เองโดยไม่ต้องพิมพ์ IP

---

## 5. Event Packet Structure (ตัวอย่าง)

```protobuf
syntax = "proto3";

package flowcursor;

message InputPacket {
  uint32 sequence = 1;
  uint64 timestamp = 2;
  string source_node_id = 3;

  oneof event {
    MouseMove move = 4;
    MouseButton button = 5;
    KeyAction key = 6;
  }
}

message MouseMove {
  int32 dx = 1;
  int32 dy = 2;
  int32 absolute_x = 3;
  int32 absolute_y = 4;
}

message MouseButton {
  uint32 button_id = 1; // 1: Left, 2: Right, 3: Middle
  bool is_down = 2;
}

message KeyAction {
  uint32 keycode = 1;
  bool is_down = 2;
  uint32 modifiers = 3; // Shift, Ctrl, Alt
}
```

---

## 6. Next Steps Checklist
1. เริ่มสร้าง Repository โครงสร้างแบบ Monorepo:
   * `/server` (สำหรับ HomeLab)
   * `/agent` (สำหรับ Windows / Linux clients)
   * `/proto` (สำหรับ Data contracts)
2. เขียน PoC (Proof of Concept) ดักจับและจำลองเมาส์บน OS หลักที่คุณใช้ก่อน
3. เชื่อมต่อ Agent 2 เครื่องผ่าน UDP echo server พื้นฐานเพื่อวัด Latency ในบ้าน