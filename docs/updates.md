# อัปเดตอัตโนมัติ

ตั้งแต่ 0.2.0 agent อัปเดตตัวเองให้ตรงกับ build ที่อยู่ใน hub
build hub image ใหม่ แล้ว `docker compose up -d` ทุกเครื่องจะอัปเดตตามเอง

## เวอร์ชัน

ทุก build มีเวอร์ชันแบบ `0.2.0+89.g712c3ea57b` (จำนวน commit และ commit hash)
ดูได้ด้วย `flow-agent --version`, `flow-agent status`, เมนู tray และคอลัมน์ Version ใน dashboard

agent ตามเวอร์ชันของ hub เสมอ ทั้งใหม่กว่าและเก่ากว่า

## ปลอดภัยแค่ไหน

ทุกปุ่มที่กดวิ่งผ่านโปรแกรมนี้ ช่องทางอัปเดตที่คนอื่นปลอมได้จึงเท่ากับฝังโปรแกรมดักคีย์บอร์ด agent จึงทำแบบนี้:

- ดาวน์โหลดจาก hub ที่ join ไว้เท่านั้น ผ่านการเชื่อมต่อ QUIC เดิมที่ยืนยันตัวตน hub แล้ว ไม่มี URL, ไม่มีเซิร์ฟเวอร์กลาง, ไม่เรียก GitHub
- ชื่อไฟล์ ขนาด และ SHA-256 มาจาก hub ทางช่องทางเดียวกัน ไฟล์ที่ขนาดหรือ hash ไม่ตรงถูกทิ้ง
- ที่ติดตั้งมาจากตารางที่กำหนดไว้ในโปรแกรม ไม่ได้มาจาก network
- dashboard (HTTP) ไม่แจกไฟล์อัปเดต

## Policy

ตั้งใน Settings → Agent updates และตั้งแยกรายเครื่องได้

| ค่า | ทำอะไร |
| :--- | :--- |
| `automatic` (ค่าเริ่มต้น) | ดาวน์โหลดทันที แล้วติดตั้งตอนเครื่องว่าง |
| `notify` | แจ้งใน tray ให้กด Update now เอง |
| `off` | ไม่ทำเอง แค่แสดงว่ามีเวอร์ชันใหม่ |

Update now ใช้ได้ทุก policy: ปุ่มใน dashboard, เมนู tray หรือ `flow-agent update`

**เครื่อง GNOME แนะนำให้ใช้ `notify`** เพราะหลังอัปเดตทุกครั้ง หน้าต่างขอสิทธิ์ Input Capture จะเด้งขึ้นมา และ agent ยังไม่ต่อ hub จนกว่าจะกดตอบ

## ติดตั้งตอนไหน

agent รอจนเครื่องว่างก่อน คือไม่ได้คุมหรือถูกคุม ไม่มีปุ่มกดค้าง และไม่มี input มา 3 วินาที แล้วค่อยติดตั้งและ restart
ใช้เวลาประมาณ 2 วินาที tray จะเปิดตัวเองใหม่เป็นเวอร์ชันใหม่ด้วย

- Linux: แทนไฟล์ที่ `install.sh` ติดตั้งไว้ แล้วเปิด agent ใหม่ใน process เดิม
- Windows: เปลี่ยนชื่อ exe ที่กำลังรันเป็น `.old` แล้ววางตัวใหม่แทน ไม่ต้องใช้ UAC ตัวเก่าถูกลบตอนเปิดครั้งถัดไป

## Rollback

ก่อนแทนไฟล์ agent เก็บสำเนาเวอร์ชันเดิมไว้ แล้วให้เวอร์ชันเดิมเฝ้าดูเวอร์ชันใหม่
ถ้าเวอร์ชันใหม่ไม่เริ่มใน 30 วินาที, crash เกิน 4 ครั้ง หรือไม่ถึง hub ใน 120 วินาที จะคืนเวอร์ชันเดิมและรายงานไปที่ dashboard
เวลาที่รอหน้าต่างสิทธิ์ของ GNOME ไม่นับ

build ที่ล้มเหลวจะไม่ถูกลองซ้ำเอง กด Update now เพื่อลองใหม่ หรือ deploy build ใหม่
log อยู่ที่ `~/.local/share/flowcursor/update/guard.log`

## ติดตั้งด้วยมือครั้งแรก

agent ก่อน 0.2.0 ไม่มีโค้ดอัปเดต ต้องติดตั้ง 0.2.0 ด้วยมือครั้งเดียว
tray และ dashboard จะแสดง *update by hand* ติดตั้งด้วย `install.sh` หรือ setup exe ทับได้เลย ไม่ต้อง join ใหม่
หลังจากนั้นอัปเดตเองทุกครั้ง

agent ที่ protocol ไม่ตรงกับ hub (0.2.0 ขึ้นไป) จะต่อเข้ามาในโหมด *update-only* ได้ อัปเดตเสร็จแล้วกลับมาใช้งานเต็ม
ถ้า policy เป็น `notify` หรือ `off` ต้องกด Update now ก่อนถึงจะใช้งานได้

## ติดตั้งจาก .deb

agent ใน `/usr/bin` ไม่เขียนทับไฟล์ระบบ dashboard จะแสดง *manual update* ให้อัปเดตด้วย `.deb` ใหม่:

```bash
sudo apt install ./flow-agent_<เวอร์ชัน>_amd64.deb
systemctl --user restart flow-agent
```

## เอา setup exe ออกจาก hub image

ใช้ติดตั้งเครื่อง Windows ใหม่โดยไม่ต้อง build เอง:

```bash
docker create --name fc flow-cursor-server:<tag>
docker cp fc:/usr/share/flowcursor/agent-builds ./agent-builds
docker rm fc
```

hub image ที่มีไฟล์ agent ขนาดประมาณ 120 MB (บีบอัดแล้วประมาณ 36 MB)
