# ติดตั้ง hub

hub (`flow-server`) รันเป็น Docker container ต้องใช้ `network_mode: host` เพราะ QUIC (UDP) และ mDNS ข้าม bridge network ของ Docker ไม่ได้

```bash
build/build.sh                      # hub image ที่มี agent ของ build เดียวกันอยู่ข้างใน
cd deploy && FLOW_ADMIN_TOKEN='รหัส-dashboard' docker compose up -d
docker compose exec flow-server flow-server --print-token
```

ใช้ image จาก `build/build.sh` เสมอ `docker compose up -d --build` ก็รันได้ แต่ได้ hub ที่ไม่มีไฟล์ agent ทำให้ agent อัปเดตตัวเองไม่ได้

## พอร์ต

| พอร์ต | ใช้ทำอะไร |
| :--- | :--- |
| UDP 7878 | agent กับ hub (QUIC) |
| TCP 7880 | dashboard |
| UDP 5353 | mDNS ให้ agent ในวงเดียวกันหา hub เอง |

agent ที่อยู่คนละวงกับ hub หา hub ด้วย mDNS ไม่เจอ ให้ระบุที่อยู่ hub ตอน join

## ข้อมูลใน volume

ทุกอย่างอยู่ใน volume `flowcursor-data` (mount ที่ `/data`):

- `hub.crt`, `hub.key`: ตัวตนของ hub
- `state.json`: เครื่องที่ join แล้ว, ผังจอ, การตั้งค่า, join token

restart, `docker compose down` แล้ว `up` หรือเปลี่ยน image ข้อมูลยังอยู่ครบ
แต่ `docker compose down -v`, `docker volume rm` หรือ `docker volume prune` ตอน hub ไม่ได้รันจะลบทิ้ง
ถ้าตัวตนของ hub หาย fingerprint จะเปลี่ยน และทุกเครื่องต้อง join ใหม่

## รหัส dashboard

ตั้งด้วย `FLOW_ADMIN_TOKEN` ถ้าไม่ตั้งหรือตั้งเป็นค่าว่าง hub จะสุ่มรหัส เก็บไว้ใน volume และพิมพ์ `dashboard token: …` ใน log ทุกครั้งที่เริ่ม

dashboard เป็น HTTP ธรรมดา คนในวง LAN เดียวกันดักรหัสได้ เหมาะกับ LAN ที่ไว้ใจได้
ถ้าไม่ไว้ใจ ให้ตั้ง `FLOW_HTTP_ADDR=127.0.0.1:7880` แล้ววาง reverse proxy ที่มี TLS ไว้ข้างหน้า เช่น Caddy:

```
hub.example.lan {
    reverse_proxy 127.0.0.1:7880
}
```

การอัปเดต agent ไม่ผ่าน dashboard จึงไม่ได้รับผลกระทบ

## ตัวแปรอื่น

| ตัวแปร | ค่าเริ่มต้น |
| :--- | :--- |
| `FLOW_DATA` | `/data` |
| `FLOW_QUIC_ADDR` | `0.0.0.0:7878` |
| `FLOW_HTTP_ADDR` | `0.0.0.0:7880` |
| `FLOW_NO_MDNS` | ไม่ตั้ง (ตั้งเป็น `1` เพื่อปิด mDNS) |
| `FLOW_NAME` | ชื่อ hub ที่ประกาศผ่าน mDNS |
| `FLOW_AGENT_BUILDS` | `/usr/share/flowcursor/agent-builds` |
| `RUST_LOG` | `info` |

## อัปเดตทั้งระบบ

build image ใหม่ แล้ว `docker compose up -d` agent ทุกเครื่องจะอัปเดตตาม ดู [อัปเดตอัตโนมัติ](updates.md)

ใช้ hub ของตัวเองได้ เพราะ agent ดึงอัปเดตจาก hub ที่ join ไว้เท่านั้น ไม่มีเซิร์ฟเวอร์กลาง
`FLOW_TAG=myhub build/build.sh` จะสร้าง image `flow-cursor-server:myhub` แล้วแก้ `image:` ใน `deploy/compose.yaml` ให้ตรง
