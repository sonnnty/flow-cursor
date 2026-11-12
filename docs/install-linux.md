# ติดตั้งบน Linux

## ก่อนติดตั้ง: เช็คเวอร์ชัน GNOME

บน Wayland การไปคุมเครื่องอื่นต้องใช้ portal InputCapture ซึ่งมีใน GNOME 45 ขึ้นไป

```bash
echo $XDG_SESSION_TYPE
gnome-shell --version
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop | grep InputCapture
```

ถ้าไม่เจอ InputCapture เครื่องนี้ยังถูกเครื่องอื่นคุมได้ แต่ไปคุมเครื่องอื่นไม่ได้
ให้ logout แล้วเลือก session แบบ Xorg ที่หน้า login backend X11 ทำได้ครบโดยไม่ต้องใช้สิทธิ์พิเศษ

## ติดตั้ง

ไฟล์อยู่ใน `build/out/linux/`

```bash
./install.sh <join token> <ที่อยู่ hub>   # ไม่ใส่ที่อยู่ hub = หาเองด้วย mDNS
./install.sh                              # ไม่ใส่ token: ใส่ทีหลังในหน้าต่าง FlowCursor
```

`install.sh` ติดตั้งในโฟลเดอร์ของผู้ใช้ ไม่ต้องใช้ root:

- `flow-agent` ที่ `~/.local/bin` รันเป็น systemd user service
- `flow-tray` เปิดเองทุกครั้งที่ login จาก `~/.config/autostart/`
- เมนูแอปชื่อ FlowCursor และไอคอน

agent ที่ติดตั้งด้วย `install.sh` อัปเดตตัวเองจาก hub ได้

ถ้าใช้ `.deb` แทน:

```bash
sudo apt install ./flow-agent_<เวอร์ชัน>_amd64.deb
systemctl --user enable --now flow-agent
```

แบบ `.deb` agent จะไม่เขียนทับไฟล์ระบบเอง ต้องอัปเดตด้วย apt ทุกครั้ง

ถอนการติดตั้ง: `./uninstall.sh` (เพิ่ม `--purge` เพื่อลบตัวตนและข้อมูล) หรือ `sudo apt remove flow-agent`

## สิทธิ์ของ GNOME (Wayland)

GNOME จะขอสิทธิ์สองอย่าง:

| หน้าต่าง | ทำให้ | ถามเมื่อไร |
| :--- | :--- | :--- |
| Remote Desktop | เครื่องอื่นคุมเครื่องนี้ และใช้คลิปบอร์ด | ครั้งแรกที่ต่อ hub ได้ ครั้งต่อไปไม่ถาม agent เก็บ restore token ไว้ |
| Input Capture | เครื่องนี้ไปคุมเครื่องอื่น | ทุกครั้งที่ agent เริ่มใหม่ เพราะ GNOME 46 ไม่มี restore token ให้สิทธิ์นี้ |

สองสิทธิ์แยกกัน ปฏิเสธอันหนึ่ง อีกอันยังใช้ได้ agent จะไม่ถามซ้ำเองจนกว่าจะ restart:

```bash
systemctl --user restart flow-agent
flow-agent forget-portal   # ลืมสิทธิ์ Remote Desktop ที่เก็บไว้ ให้ถามใหม่
```

ถ้า portal หลุดระหว่างใช้งาน agent คืนเมาส์ให้เครื่องนี้ทันที แล้วสร้าง session ใหม่เอง
ระยะรอเริ่มที่ 1 วินาที และเพิ่มเป็นสองเท่าทุกครั้งที่ไม่สำเร็จ สูงสุด 60 วินาที

## ไอคอน "มีคนควบคุมเครื่องนี้" บนแถบด้านบน

GNOME แสดงไอคอนนี้ตลอดเวลาที่มี session Remote Desktop เปิดอยู่
agent จึงเปิด session นี้เฉพาะตอนที่เครื่องอื่นคุมเครื่องนี้ได้จริง คือ:

- ต่อ hub ได้ (ไม่ใช่โหมด update-only)
- อยู่ในวงที่อนุญาต (ไม่ away)
- ไม่ได้กดหยุดแชร์ (pause)

ถ้าข้อใดไม่จริง agent ปล่อยปุ่มและคีย์ที่เครื่องอื่นกดค้างไว้ก่อน แล้วปิด session ไอคอนก็หายไป
ตอนกด pause, away หรือหยุด agent จะปิดทันที
ตอน hub หลุด จะรอ 10 วินาทีก่อนปิด เพราะ agent ต่อ hub ใหม่ภายในเวลานี้ได้เกือบทุกครั้ง (เช่น hub restart) ไอคอนจึงไม่กะพริบ
พอต่อ hub ได้อีก agent เปิด session ใหม่ด้วย restore token เดิม ไม่มีหน้าต่างขอสิทธิ์

ระหว่างที่ session ปิด:

- เครื่องอื่นมองเห็นเครื่องนี้ว่าคุมไม่ได้ และจะไม่ข้ามจอมา
- คลิปบอร์ดผ่าน portal ปิดไปด้วย (ตอนนั้นไม่ได้ต่อ hub หรือหยุดแชร์อยู่แล้ว)
- เครื่องนี้ยังไปคุมเครื่องอื่นได้ตามปกติ เพราะใช้ Input Capture ซึ่งไม่เกี่ยวกับ session นี้

**ข้อแลกเปลี่ยน:** ถ้า hub ล่มนานเกิน 10 วินาที เครื่อง GNOME บน Wayland จะถูกคุมผ่าน P2P ไม่ได้จนกว่า hub กลับมา
(ก่อนหน้านี้ยังถูกคุมได้) แต่ยังไปคุมเครื่องอื่นได้ เครื่อง Xorg และ Windows ไม่มีไอคอนนี้ จึงทำงานเหมือนเดิม

## คลิปบอร์ดบน GNOME

GNOME ไม่ให้โปรแกรมเบื้องหลังอ่านคลิปบอร์ด Wayland ตรงๆ agent จึงใช้ตามลำดับ:

1. Clipboard portal ที่มากับสิทธิ์ Remote Desktop (GNOME 45 ขึ้นไป) ได้ทั้งข้อความและรูป ใช้ได้เฉพาะตอนที่ session Remote Desktop เปิดอยู่
2. คลิปบอร์ด X11 ผ่าน XWayland ใช้ได้เฉพาะตอนที่ XWayland ทำงานอยู่
3. บน Xorg ใช้คลิปบอร์ด X11 ตรงๆ

## ไอคอนบนแถบด้านบน

GNOME ไม่มี system tray ในตัว ต้องเปิด extension "AppIndicator and KStatusNotifierItem Support"
Zorin OS และ Ubuntu เปิดไว้ให้แล้ว เช็คด้วย `gnome-extensions list --enabled | grep -i appindicator`
ถ้าไม่มี ติดตั้งด้วย `sudo apt install gnome-shell-extension-appindicator` แล้ว logout และ login ใหม่

KDE, Xfce, Cinnamon และ MATE แสดงไอคอนได้เลย ถ้าไม่มีที่แสดงไอคอน ยังเปิดหน้าต่างสถานะจากเมนูแอปได้

## Backend

`backend = "auto"` เลือก `wayland` เมื่อ `XDG_SESSION_TYPE=wayland` นอกนั้นใช้ `x11`
บังคับได้ใน [ไฟล์ config](settings.md#ไฟล์-config-ของ-agent) หรือ `flow-agent run --backend x11`

## ดูสถานะ

```bash
flow-agent status
systemctl --user status flow-agent
journalctl --user -u flow-agent -f
```
