# ติดตั้งบน Windows

ใช้ `FlowCursor-setup-<เวอร์ชัน>.exe` จาก `build/out/windows/` ดับเบิลคลิกแล้วทำตามขั้นตอน

1. SmartScreen จะเตือนครั้งแรก เพราะไฟล์ไม่ได้ sign ให้กด **More info** แล้วกด **Run anyway**
2. เลือกส่วนประกอบ
   - **Start FlowCursor when I sign in**: เปิดเองตอน login (เปิดไว้เป็นค่าเริ่มต้น)
   - **Allow direct connections in Windows Firewall**: เปิด UDP 7879 บนเครือข่าย Private และ Domain สำหรับ P2P
     ขั้นนี้ขั้นเดียวที่ถาม UAC ถ้าไม่เลือก Windows อาจถามเรื่อง firewall ตอน agent เริ่มครั้งแรกแทน ให้เลือก Private
3. หน้า **Join your FlowCursor hub**: วาง join token และที่อยู่ hub จะเว้นว่างแล้วใส่ทีหลังในหน้าต่าง FlowCursor ก็ได้
4. หน้าสุดท้ายกด **Start FlowCursor now** ไอคอนจะขึ้นที่มุมขวาล่าง ถ้าไม่เห็นให้ดูในลูกศร ^

ติดตั้งทับเวอร์ชันเดิมได้ การตั้งค่าและตัวตนของเครื่องยังอยู่ ไม่ต้อง join ใหม่

## ติดตั้งที่ไหน

ติดตั้งเฉพาะผู้ใช้คนนี้ ไม่ต้องใช้สิทธิ์ admin

| อะไร | ที่ไหน |
| :--- | :--- |
| โปรแกรม | `%LOCALAPPDATA%\Programs\FlowCursor\` |
| config | `%APPDATA%\flowcursor\config\agent.toml` |
| log | `%LOCALAPPDATA%\flowcursor\data\agent.log` |
| เปิดตอน login | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |

ตอน login จะเปิด `flow-tray.exe` แล้ว tray เปิด `flow-agent.exe` ต่อ
ถ้า agent หยุด tray จะเปิดให้ใหม่ ไม่ถี่กว่าทุก 30 วินาที

ถอนการติดตั้ง: Settings → Apps → FlowCursor → Uninstall
ติ๊ก *Also delete settings, identity and logs* ถ้าต้องการลบข้อมูลด้วย

## ติดตั้งแบบสคริปต์

```powershell
.\FlowCursor-setup-<เวอร์ชัน>.exe /S /TOKEN=<join token> /HUB=<ที่อยู่ hub> /FIREWALL
```

| ตัวเลือก | ผล |
| :--- | :--- |
| `/S` | ติดตั้งแบบไม่มีหน้าต่าง |
| `/TOKEN=`, `/HUB=` | join hub ทันที |
| `/FIREWALL` | เพิ่ม firewall rule (ถาม UAC ถ้าไม่ได้รันแบบ admin) |
| `/NOAUTOSTART` | ไม่เปิดเองตอน login |
| `/NORUN` | ไม่เปิดหลังติดตั้ง |
| `/D=C:\path` | เปลี่ยนที่ติดตั้ง ต้องอยู่ท้ายสุด |

exit code 0 คือสำเร็จ 2 คือติดตั้งแล้วแต่ join ไม่สำเร็จ
ถอนแบบไม่มีหน้าต่าง: `"%LOCALAPPDATA%\Programs\FlowCursor\uninstall.exe" /S` (เพิ่ม `/PURGE` เพื่อลบข้อมูล)

## โหมด elevated

agent ที่รันแบบผู้ใช้ปกติคุมหน้าต่างของโปรแกรมที่รันแบบ admin ไม่ได้ ถ้าต้องการ ให้ติดตั้งด้วย `install.ps1` ใน PowerShell แบบ admin:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Token <join token> -Hub <ที่อยู่ hub> -Elevated
```

agent จะรันผ่าน Scheduled Task ด้วยสิทธิ์สูง ส่วน tray แสดงสถานะอย่างเดียว ถอนด้วย `.\uninstall.ps1`

หน้าต่าง UAC, หน้าจอล็อก และ Ctrl+Alt+Del ยังคุมไม่ได้ทุกโหมด เพราะต้องมี Windows service ที่รันเป็น SYSTEM ซึ่งยังไม่ได้ทำ
