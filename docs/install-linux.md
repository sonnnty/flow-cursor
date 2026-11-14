# ติดตั้งบน Linux

## ก่อนติดตั้ง: เช็คเวอร์ชัน GNOME

บน Wayland การไปคุมเครื่องอื่นต้องใช้ InputCapture ซึ่งมีใน GNOME 45 ขึ้นไป
(บน GNOME agent ใช้ของ mutter โดยตรง ดู [สิทธิ์ของ GNOME](#สิทธิ์ของ-gnome-wayland) ระบบอื่นใช้ portal)

```bash
echo $XDG_SESSION_TYPE
gnome-shell --version
busctl --user list | grep -E 'org.gnome.Mutter.(InputCapture|RemoteDesktop)'
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop | grep InputCapture
```

ถ้าไม่เจอ InputCapture ทั้งสองแบบ เครื่องนี้ยังถูกเครื่องอื่นคุมได้ แต่ไปคุมเครื่องอื่นไม่ได้
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

บน GNOME agent คุยกับ mutter (ตัวจัดการหน้าต่างของ GNOME) โดยตรงผ่าน D-Bus `org.gnome.Mutter.InputCapture` และ `org.gnome.Mutter.RemoteDesktop`
เป็นตัวเดียวกับที่ portal ของ GNOME เรียกอยู่ข้างหลังหลังจากถามสิทธิ์ ทางนี้เรียกว่า **Mutter direct** และเป็นค่าเริ่มต้น จึงไม่มีหน้าต่างขอสิทธิ์เลย

| สิทธิ์ | ทำให้ | Mutter direct (GNOME) | portal (KDE และอื่นๆ หรือ `gnome_direct = "off"`) |
| :--- | :--- | :--- | :--- |
| Remote Desktop | เครื่องอื่นคุมเครื่องนี้ และใช้คลิปบอร์ด | ไม่ถาม | ถามครั้งแรกที่ต่อ hub ได้ ครั้งต่อไปไม่ถาม agent เก็บ restore token ไว้ |
| Input Capture | เครื่องนี้ไปคุมเครื่องอื่น | ไม่ถาม | ถามทุกครั้งที่ agent เริ่มใหม่ อัปเดต เปลี่ยนผังจอ เปลี่ยนจอ หรือพับจอแล้วเปิด เพราะ GNOME 46 ไม่มี restore token ให้สิทธิ์นี้ |

ข้อควรรู้เรื่อง Mutter direct:

- เป็น API ภายในของ GNOME ไม่ใช่ API สาธารณะ โปรแกรมไหนก็ตามที่รันใน session ของผู้ใช้เรียกได้เหมือนกัน นี่คือเหตุที่ไม่มีหน้าต่างถาม
- GNOME รุ่นหน้าอาจเปลี่ยนหรือเอา API นี้ออก ถ้าเรียกไม่ได้ agent ใช้ portal เอง แล้วหน้าต่างขอสิทธิ์จะกลับมา
- GNOME ยังแสดงไอคอน "มีคนควบคุมเครื่องนี้" ตอน session Remote Desktop เปิดเหมือนเดิม
- ปิดได้ด้วย `gnome_direct = "off"` ใน [ไฟล์ config](settings.md#ไฟล์-config-ของ-agent) แล้ว restart agent

ดูว่าใช้ทางไหนอยู่ด้วย `flow-agent status` บรรทัด `input capture: enabled (mutter direct), ...` และ `remote control: mutter direct`
หรือ log ตอนเริ่ม (`input capture through mutter directly`, `remote control through mutter directly`)
ถ้าเรียก mutter ไม่ได้ log บอกเหตุผลในบรรทัด `mutter's own ... interface is not usable`

เช็คทาง Mutter direct บนเครื่องจริงโดยไม่ต้องต่อ hub ได้ด้วย `flow-agent gnome-probe`
คำสั่งนี้สร้าง session ทั้งสองแบบ พิมพ์จอ อุปกรณ์ EI และคลิปบอร์ดที่ได้ แล้วปิดทั้งหมด ไม่จับเมาส์
ไอคอน "มีคนควบคุมเครื่องนี้" ขึ้นประมาณหนึ่งวินาทีระหว่างเช็ค ถ้ามีส่วนไหนใช้ไม่ได้ จะออกด้วย exit code 1

ทาง portal: สองสิทธิ์แยกกัน ปฏิเสธอันหนึ่ง อีกอันยังใช้ได้ agent จะไม่ถามซ้ำเองจนกว่าจะ restart:

```bash
systemctl --user restart flow-agent
flow-agent forget-portal   # ลืมสิทธิ์ Remote Desktop ที่เก็บไว้ ให้ถามใหม่ (ทาง portal เท่านั้น)
```

ถ้า session หลุดระหว่างใช้งาน (ทั้งสองทาง) agent คืนเมาส์ให้เครื่องนี้ทันที แล้วสร้าง session ใหม่เอง
ระยะรอเริ่มที่ 1 วินาที และเพิ่มเป็นสองเท่าทุกครั้งที่ไม่สำเร็จ สูงสุด 60 วินาที

### เปลี่ยนจอแล้วข้ามจอไม่ได้

ทุกครั้งที่การตั้งค่าจอเปลี่ยน (เสียบหรือถอดจอ เปลี่ยนความละเอียดหรือ scale พับจอ laptop บางครั้งตอนตื่นจาก sleep)
GNOME 46 ปิด session Input Capture เงียบๆ ขอบจอที่ใช้ข้ามเครื่องหายไปโดยไม่มีสัญญาณบอกนอกจาก ZonesChanged (ซึ่ง portal 1.18 ไม่ส่งต่อ)
session ที่ถูกปิดแบบนี้เปิดใหม่ได้ แต่ GNOME ทิ้งอุปกรณ์ที่ส่ง input ให้ agent ไปแล้ว จึงต้องสร้าง session ใหม่
agent รู้จาก ZonesChanged หรือสัญญาณ `MonitorsChanged` ของ GNOME (หรือเช็คทุก 10 วินาที) ส่งจอใหม่ให้ hub แล้วสร้าง session ใหม่ให้เองครั้งเดียวต่อการเปลี่ยน
(รอขอบจอของผังใหม่ไม่เกิน 2 วินาที)

- **Mutter direct**: ไม่มีหน้าต่างถาม ข้ามจอได้อีกเองภายในไม่กี่วินาที
  agent สร้าง session ใหม่เงียบๆ ด้วยหลังตื่นจาก sleep (เช่น เปิดฝา laptop) และหลังกด shortcut ยกเลิกการจับเมาส์ของ GNOME
- **portal**: GNOME ถามสิทธิ์ Input Capture อีกครั้ง ให้กด Allow ถ้าเครื่องนี้ไม่มีขอบไปเครื่องอื่น จะไม่ถาม

นอกจากนี้ agent เช็คทุก 30 วินาที และทุกครั้งที่ hub ส่งผังจอเดิมซ้ำ ว่าขอบจอยังเป็นของชุดจอปัจจุบัน ถ้าไม่ใช่ก็สร้างใหม่

ดูสถานะได้จาก `flow-agent status` บรรทัด `input capture`, `last edge hit` และ `capture change`
หรือแถว Edge capture ในหน้าต่างสถานะ เช่น `enabled (mutter direct), 1 barrier, zone set 7, session 3`
ทาง portal ถ้าค้างที่ `opening` แปลว่ารอหน้าต่างขอสิทธิ์ของ GNOME อยู่

## ไอคอน "มีคนควบคุมเครื่องนี้" บนแถบด้านบน

GNOME แสดงไอคอนนี้ตลอดเวลาที่มี session Remote Desktop เปิดอยู่
agent จึงเปิด session นี้เฉพาะตอนที่เครื่องอื่นคุมเครื่องนี้ได้จริง คือ:

- ต่อ hub ได้ (ไม่ใช่โหมด update-only)
- อยู่ในวงที่อนุญาต (ไม่ away)
- ไม่ได้กดหยุดแชร์ (pause)

ถ้าข้อใดไม่จริง agent ปล่อยปุ่มและคีย์ที่เครื่องอื่นกดค้างไว้ก่อน แล้วปิด session ไอคอนก็หายไป
ตอนกด pause, away หรือหยุด agent จะปิดทันที
ตอน hub หลุด จะรอ 10 วินาทีก่อนปิด เพราะ agent ต่อ hub ใหม่ภายในเวลานี้ได้เกือบทุกครั้ง (เช่น hub restart) ไอคอนจึงไม่กะพริบ
พอต่อ hub ได้อีก agent เปิด session ใหม่ ไม่มีหน้าต่างขอสิทธิ์ (Mutter direct ไม่ต้องขอ ทาง portal ใช้ restore token เดิม)

ระหว่างที่ session ปิด:

- เครื่องอื่นมองเห็นเครื่องนี้ว่าคุมไม่ได้ และจะไม่ข้ามจอมา
- คลิปบอร์ดของ session นี้ปิดไปด้วย (ตอนนั้นไม่ได้ต่อ hub หรือหยุดแชร์อยู่แล้ว)
- เครื่องนี้ยังไปคุมเครื่องอื่นได้ตามปกติ เพราะใช้ Input Capture ซึ่งไม่เกี่ยวกับ session นี้

**ข้อแลกเปลี่ยน:** ถ้า hub ล่มนานเกิน 10 วินาที เครื่อง GNOME บน Wayland จะถูกคุมผ่าน P2P ไม่ได้จนกว่า hub กลับมา
(ก่อนหน้านี้ยังถูกคุมได้) แต่ยังไปคุมเครื่องอื่นได้ เครื่อง Xorg และ Windows ไม่มีไอคอนนี้ จึงทำงานเหมือนเดิม

## คลิปบอร์ดบน GNOME

GNOME ไม่ให้โปรแกรมเบื้องหลังอ่านคลิปบอร์ด Wayland ตรงๆ agent จึงใช้ตามลำดับ:

1. คลิปบอร์ดของ session Remote Desktop (Mutter direct หรือ Clipboard portal บน GNOME 45 ขึ้นไป) ได้ทั้งข้อความและรูป ใช้ได้เฉพาะตอนที่ session Remote Desktop เปิดอยู่
2. คลิปบอร์ด X11 ผ่าน XWayland ใช้ได้เฉพาะตอนที่ XWayland ทำงานอยู่
3. บน Xorg ใช้คลิปบอร์ด X11 ตรงๆ

copy หนึ่งครั้งใน GNOME ทำให้ GNOME ส่งสัญญาณมา 2 ถึง 3 ครั้ง agent อ่านคลิปบอร์ดครั้งเดียวจากสัญญาณล่าสุด
คลิปบอร์ดจากเครื่องอื่นที่มาถึงตอน session ยังเปิดไม่เสร็จ (เช่น เพิ่งต่อ hub ได้) จะถูกเก็บไว้แล้วใส่ให้ทันทีที่ session เปิด ถ้ารอไม่เกิน 60 วินาที

ดู log ของคลิปบอร์ดด้วย `log_level = "info,flow_agent=debug"` แล้วหาบรรทัด `clipboard owner changed` และ `local clipboard read`

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

ระดับ log มาจาก dashboard (Settings → Agent log level) unit ของ systemd ไม่ตั้ง `RUST_LOG` แล้ว
ถ้าตั้ง `RUST_LOG` เอง (`systemctl --user edit flow-agent` แล้วใส่ `Environment=RUST_LOG=debug`) หรือ `log_level` ใน agent.toml ค่านั้นจะชนะค่าจาก dashboard
ดูลำดับได้ที่ [Dashboard และการตั้งค่า](settings.md#ระดับ-log-ของ-agent)
