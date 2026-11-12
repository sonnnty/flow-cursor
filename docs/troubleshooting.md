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
| เมาส์ไม่ข้ามจอ | ขอบจอในผังต้องชิดกัน, เครื่องปลายทางต้อง online และเปิดใช้งาน, บน Wayland ต้องอนุญาต Input Capture, ถ้าตั้งปุ่มข้ามจอต้องกดค้าง |
| เครื่องเป็น `away` ทั้งที่อยู่บ้าน | เทียบ IP ใน dashboard กับวงที่อนุญาต ต้องใส่วงของเครื่อง ไม่ใช่วงของ server |
| log `permission (... portal) was refused` | กดปฏิเสธหน้าต่างของ GNOME ไป ให้ `systemctl --user restart flow-agent` แล้วกด Allow |
| ปุ่มค้าง | กด emergency hotkey เครื่องที่ถูกคุมจะปล่อยปุ่มเองใน 2.5 วินาทีถ้าต้นทางหายไป |
| ไม่เห็นไอคอนบน GNOME | เปิด extension AppIndicator ดู [ติดตั้งบน Linux](install-linux.md#ไอคอนบนแถบด้านบน) |
| ไอคอนสีเทา แต่ agent รันอยู่ | ต่อ hub ไม่ได้ ดูสาเหตุในหน้าต่างสถานะ |
| `another flow-agent of this user is already running` | มี agent รันอยู่แล้ว ใช้ `flow-agent status` หรือ `flow-agent stop` |
| Windows: ไอคอนหายหลังกด Quit | Quit ปิดทั้งไอคอนและ agent เปิดใหม่จาก Start menu |
| ต้องการ log ละเอียด | Settings → log level เป็น `debug` |

## ข้อจำกัด

**GNOME (Wayland)**

- การไปคุมเครื่องอื่นต้องใช้ GNOME 45 ขึ้นไป เก่ากว่านั้นใช้ session แบบ Xorg
- หน้าต่าง Input Capture ถามทุกครั้งที่ agent เริ่มใหม่หรืออัปเดต
- ปุ่มข้ามจอต้องกดหลังเมาส์ชนขอบแล้ว
- เคอร์เซอร์ตอนคุมเครื่องอื่นอยู่ที่ขอบจอได้อย่างเดียว
- ถ้าเครื่องปลายทางไม่มีอุปกรณ์ pointer แบบ absolute การเลื่อนจะคลาดเล็กน้อยตาม pointer acceleration

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

- การอัปเดตตัวเองบน Windows และการ restart ผ่าน systemd จริง
- การย้ายและซ่อนเคอร์เซอร์บน Windows และ X11
- ตัวติดตั้งและหน้าต่างบน Windows
