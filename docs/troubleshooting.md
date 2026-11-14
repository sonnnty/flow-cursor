# แก้ปัญหาและข้อจำกัด

## อาการที่พบบ่อย

| อาการ | ทำอย่างไร |
| :--- | :--- |
| `invalid peer certificate` | fingerprint ของ hub ไม่ตรง hub ถูกสร้างใหม่ (volume หาย) หรือใช้ token ของ hub อื่น ให้ `flow-agent join <token ใหม่>` |
| `hub rejected this agent: invalid join token` | token ถูกสร้างใหม่แล้ว ใช้ token ล่าสุดจาก dashboard |
| `too old to update itself` | agent ก่อน 0.2.0 ติดตั้งเวอร์ชันของ hub ด้วยมือครั้งเดียว ดู [อัปเดตอัตโนมัติ](updates.md#ติดตั้งด้วยมือครั้งแรก) |
| tray ขึ้น *connected only to update* | protocol ไม่ตรงกับ hub ถ้า policy ไม่ใช่ automatic ให้กด Update now |
| *installed by package* | ติดตั้งจาก `.deb` อัปเดตด้วย `sudo apt install ./flow-agent_<เวอร์ชัน>_amd64.deb` |
| อัปเดตค้างที่ *installs when this machine is idle* | ยังคุมหรือถูกคุมอยู่ หรือมีปุ่มกดค้าง ปล่อยแล้วรอ 3 วินาที |
| *last update failed* | ดูเหตุผลใน dashboard หรือ `flow-agent status` แก้แล้วกด Update now |
| dashboard ขึ้น *no agent builds in this hub* | hub สร้างด้วย `compose --build` ให้ใช้ image จาก `build/build.sh` |
| `this agent was removed from the hub` | เครื่องถูกยกเลิกสิทธิ์ใน dashboard ต้อง join ใหม่ |
| เส้นทางเป็น `relay` ตลอด | firewall บล็อก UDP 7879 ระหว่างเครื่อง หรือที่อยู่ที่ประกาศผิด ตั้ง `p2p_advertise` |
| dashboard เตือน *overlap* หรือ *does not touch any other screen* | ลากเครื่องให้ขอบชิดกันโดยไม่ทับ แล้วกด Save layout ดู [เมื่อจอของเครื่องเปลี่ยน](settings.md#เมื่อจอของเครื่องเปลี่ยน) |
| tray ขึ้น *No edges to other machines* | เครื่องนี้ไม่ชิดเครื่องไหนในผังจอ แก้ใน dashboard |
| เสียบหรือถอดจอแล้ว dashboard ยังแสดงจอเก่า | agent ส่งจอใหม่เองภายในไม่กี่วินาที (ถ้า GNOME ไม่มี DisplayConfig ภายใน 10 วินาที) ถ้ายังไม่เปลี่ยน ดู log ของ agent ว่ามี `monitor configuration changed` ไหม |
| GNOME ถาม Input Capture หรือ Remote Desktop | agent ใช้ทาง portal อยู่ ดู `flow-agent status` บรรทัด `input capture` และ `remote control` ถ้าเป็น `(portal)` ให้หาเหตุผลใน log บรรทัด `mutter's own ... interface is not usable` และเช็คว่าไม่ได้ตั้ง `gnome_direct = "off"` ดู [สิทธิ์ของ GNOME](install-linux.md#สิทธิ์ของ-gnome-wayland) |
| GNOME ถาม Input Capture อีกหลังเสียบหรือถอดจอ (ทาง portal) | ปกติของทาง portal: GNOME ปิด session จับเมาส์ทุกครั้งที่จอเปลี่ยน agent เปิด session ใหม่ครั้งเดียวต่อการเปลี่ยน กด Allow ทาง Mutter direct ไม่ถาม |
| GNOME: เคยข้ามจอได้ แล้วชนขอบเฉยๆ | ดู `flow-agent status` บรรทัด `input capture` ทาง portal ถ้าเป็น `opening` ให้กด Allow ในหน้าต่างของ GNOME ที่ค้างอยู่ ทาง Mutter direct ถ้าไม่เป็น `enabled` ภายในไม่กี่วินาที ดู log บรรทัด `input capture` ดู [เปลี่ยนจอแล้วข้ามจอไม่ได้](install-linux.md#เปลี่ยนจอแล้วข้ามจอไม่ได้) |
| GNOME: copy แล้วเครื่องอื่นไม่ได้ | `flow-agent status` ต้องมี clipboard ถ้าไม่มี session Remote Desktop อาจปิดอยู่ (pause, away, ไม่ได้ต่อ hub) |
| เมาส์ไม่ข้ามจอ | ขอบจอในผังต้องชิดกัน, เครื่องปลายทางต้อง online และเปิดใช้งาน, บน Wayland ต้องอนุญาต Input Capture, เครื่องปลายทาง GNOME ต้องเปิด session Remote Desktop เสร็จแล้ว (dashboard แสดง inject), ถ้าตั้งปุ่มข้ามจอต้องกดค้าง |
| เครื่องเป็น `away` ทั้งที่อยู่บ้าน | เทียบ IP ใน dashboard กับวงที่อนุญาต ต้องใส่วงของเครื่อง ไม่ใช่วงของ server |
| log `permission (... portal) was refused` | ทาง portal: กดปฏิเสธหน้าต่างของ GNOME ไป ให้ `systemctl --user restart flow-agent` แล้วกด Allow |
| ปุ่มค้าง | กด emergency hotkey เครื่องที่ถูกคุมจะปล่อยปุ่มเองใน 2.5 วินาทีถ้าต้นทางหายไป |
| ไม่เห็นไอคอนบน GNOME | เปิด extension AppIndicator ดู [ติดตั้งบน Linux](install-linux.md#ไอคอนบนแถบด้านบน) |
| ไอคอนหายไปทั้งที่ `flow-tray` ยังรัน | tray ลงทะเบียนใหม่เองภายใน 30 วินาที ถ้ายังไม่ขึ้นให้เปิด extension AppIndicator หรือรัน `flow-tray --quit` แล้วเปิดใหม่ |
| ไอคอนสีเหลือง หน้าต่างบอกว่าเครื่องอื่นควบคุมไม่ได้ | ปกติ: อยู่นอกวงที่อนุญาตหรือกด Pause อยู่ จะควบคุมได้อีกเมื่อกลับเข้าวงหรือกด Resume |
| ไอคอนสีเทา แต่ agent รันอยู่ | ต่อ hub ไม่ได้ ดูสาเหตุในหน้าต่างสถานะ |
| `another flow-agent of this user is already running` | มี agent รันอยู่แล้ว ใช้ `flow-agent status` หรือ `flow-agent stop` |
| Windows: ไอคอนหายหลังกด Quit | Quit ปิดทั้งไอคอนและ agent เปิดใหม่จาก Start menu |
| ต้องการ log ละเอียด | Settings → log level เป็น `debug` ถ้าไม่มีผล agent มี `RUST_LOG` หรือ `log_level` ใน agent.toml อยู่ ดู [ระดับ log](settings.md#ระดับ-log-ของ-agent) |

## ข้อจำกัด

**GNOME (Wayland)**

- การไปคุมเครื่องอื่นต้องใช้ GNOME 45 ขึ้นไป เก่ากว่านั้นใช้ session แบบ Xorg
- Mutter direct (ค่าเริ่มต้นบน GNOME) ใช้ API ภายในของ GNOME ไม่ใช่ API สาธารณะ โปรแกรมไหนใน session ของผู้ใช้ก็เรียกได้ จึงไม่มีหน้าต่างถาม
  GNOME รุ่นหน้าอาจเปลี่ยน API นี้ ถ้าเรียกไม่ได้ agent ใช้ portal แทน และหน้าต่างขอสิทธิ์จะกลับมา
- ทาง portal: หน้าต่าง Input Capture ถามทุกครั้งที่ agent เริ่มใหม่ อัปเดต เปลี่ยนผังจอ หรือเปลี่ยนจอ (เฉพาะเมื่อเครื่องนี้มีขอบไปเครื่องอื่น)
  GNOME 46 ปิด session เดิมเมื่อจอเปลี่ยน และใช้ซ้ำไม่ได้ ถ้าไม่มีขอบ agent จะยังไม่เปิด session ใหม่
- dashboard แสดงขนาดจอเป็น logical pixel เช่นจอ 1920x1200 ที่ scale 125% แสดงเป็น 1536x960 ซึ่งถูกต้องสำหรับผังจอ
  การแสดงความละเอียดจริงคู่กันต้องเพิ่ม field ใน protocol ซึ่งทำให้ทุก agent ต้องอัปเดตก่อนใช้งานได้ จึงยังไม่ได้ทำ
- ระหว่างล็อกจอ GNOME ไม่ให้จับเมาส์หรือคุมเครื่อง เครื่องนี้ไปคุมเครื่องอื่นและถูกคุมไม่ได้ หลังปลดล็อกกลับมาเองภายในประมาณ 1 วินาที
- ปุ่มข้ามจอต้องกดหลังเมาส์ชนขอบแล้ว
- เคอร์เซอร์ตอนคุมเครื่องอื่นอยู่ที่ขอบจอได้อย่างเดียว
- ถ้าเครื่องปลายทางไม่มีอุปกรณ์ pointer แบบ absolute การเลื่อนจะคลาดเล็กน้อยตาม pointer acceleration
- ถ้า hub ล่มเกิน 10 วินาที เครื่องอื่นคุมเครื่องนี้ไม่ได้จนกว่า hub กลับมา ดู [ติดตั้งบน Linux](install-linux.md#ไอคอน-มีคนควบคุมเครื่องนี้-บนแถบด้านบน)

**Windows**

- คุมหน้าต่าง UAC, หน้าจอล็อก และ Ctrl+Alt+Del ไม่ได้ ต้องมี service ที่รันเป็น SYSTEM ซึ่งยังไม่ได้ทำ
- คุมหน้าต่างของโปรแกรมที่รันแบบ admin ได้เฉพาะเมื่อติดตั้งแบบ `-Elevated`
- เมาส์ใช้ค่า raw ไม่มี acceleration ของ Windows ปรับความเร็วเมาส์ของเครื่องนั้นใน dashboard
- ตัวติดตั้งไม่ได้ sign SmartScreen เตือนครั้งแรก และ antivirus บางตัวอาจสงสัย
- โหมด `-Elevated`: rollback เปิด agent เวอร์ชันเก่าแบบ process ธรรมดา ไม่ผ่าน Scheduled Task

**ทั่วไป**

- คลิปบอร์ดส่งไปทุกเครื่องทันทีที่ copy รูปส่งเป็น PNG บน X11 จำกัดขนาดประมาณ 16 MiB
- dashboard เป็น HTTP ธรรมดา และ hub เห็น input ที่ relay ภายใน process ของตัวเอง
- วงที่อนุญาตตัดสินจาก IP อย่างเดียว เครือข่ายอื่นที่ใช้ช่วงเดียวกับที่บ้านจะนับว่าอยู่บ้าน
- หน้าต่างสถานะเป็นภาษาอังกฤษ

**ยังไม่เคยรันบนเครื่องจริง** (ผ่านแค่ test กับ agent จำลอง และ test ใน Docker)

- Mutter direct ทั้งหมด: session จับเมาส์และ Remote Desktop ของ mutter, คลิปบอร์ดของ mutter, การสร้าง session ใหม่หลังจอเปลี่ยน ตื่นจาก sleep ปลดล็อกจอ และ shortcut ยกเลิกของ GNOME, การกลับไปใช้ portal
  (ทดสอบกับ mutter จำลองบน dbus-daemon เท่านั้น เช็คบนเครื่องจริงด้วย `flow-agent gnome-probe`)
- การรู้ว่าจอเปลี่ยนบน GNOME (สัญญาณ DisplayConfig, ZonesChanged, เช็คทุก 10 วินาที) การเปิด session จับเมาส์ใหม่หลังจอเปลี่ยน และการเช็คตัวเองทุก 30 วินาที
- การอ่านสัญญาณคลิปบอร์ดของ GNOME เอง (ทดสอบกับ portal จำลองบน dbus-daemon เท่านั้น)
- การเช็คจอซ้ำหลัง `WM_DISPLAYCHANGE` บน Windows และ event RandR แบบ CRTC/output บน X11

- การอัปเดตตัวเองบน Windows และการ restart ผ่าน systemd จริง
- การย้ายและซ่อนเคอร์เซอร์บน Windows และ X11
- ตัวติดตั้งและหน้าต่างบน Windows
