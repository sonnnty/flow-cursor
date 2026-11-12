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

ครั้งแรกที่ agent เริ่ม GNOME จะขอสิทธิ์สองอย่าง:

| หน้าต่าง | ทำให้ | ถามซ้ำไหม |
| :--- | :--- | :--- |
| Remote Desktop | เครื่องอื่นคุมเครื่องนี้ และใช้คลิปบอร์ด | ไม่ถาม agent เก็บ restore token ไว้ |
| Input Capture | เครื่องนี้ไปคุมเครื่องอื่น | ถามทุกครั้งที่ agent เริ่มใหม่ เพราะ GNOME 46 ไม่มี restore token ให้สิทธิ์นี้ |

สองสิทธิ์แยกกัน ปฏิเสธอันหนึ่ง อีกอันยังใช้ได้ agent จะไม่ถามซ้ำเองจนกว่าจะ restart:

```bash
systemctl --user restart flow-agent
flow-agent forget-portal   # ลืมสิทธิ์ Remote Desktop ที่เก็บไว้ ให้ถามใหม่
```

ถ้า portal หลุดระหว่างใช้งาน agent คืนเมาส์ให้เครื่องนี้ทันที แล้วสร้าง session ใหม่เอง
ระยะรอเริ่มที่ 1 วินาที และเพิ่มเป็นสองเท่าทุกครั้งที่ไม่สำเร็จ สูงสุด 60 วินาที

## คลิปบอร์ดบน GNOME

GNOME ไม่ให้โปรแกรมเบื้องหลังอ่านคลิปบอร์ด Wayland ตรงๆ agent จึงใช้ตามลำดับ:

1. Clipboard portal ที่มากับสิทธิ์ Remote Desktop (GNOME 45 ขึ้นไป) ได้ทั้งข้อความและรูป
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
