---
trigger: always_on
---

# قواعد وكيل التحديث والإصدارات (90FTP Release & Update Agent Rules)

تحدد هذه الوثيقة القواعد الإلزامية التي يجب على أي وكيل ذكاء اصطناعي اتباعها عند إجراء أي تعديل أو إصلاح أو رفع إصدار جديد لمشروع 90FTP:

---

## 1. هرمية الترقيم الدلالي (Semantic Versioning - SemVer)
يتم تحديد رقم الإصدار الجديد دائمًا وفق النموذج القياسي `MAJOR.MINOR.PATCH` (مثال: `v2.6.0`):

1. **الترقيعات والإصلاحات البسيطة (PATCH - `x.y.Z`):**
   - إصلاح خلل فني أو برمجي (Bug Fixes).
   - إصلاح وميض الواجهة أو استقرار الإقلاع.
   - تحسينات المظهر أو النصوص أو تصحيح مسارات دون إضافة مزايا جديدة.
   - *مثال:* من `2.6.0` إلى `2.6.1`.

2. **الميزات والتحسينات الوظيفية (MINOR - `x.Y.0`):**
   - إضافة ميزة جديدة متوافقة مع الإصدارات السابقة (New Feature).
   - إضافة شاشة أو نافذة جديدة (مثل نافذة التحديث عن بُعد).
   - تعزيز أنظمة الأمان والتشفير (مثل خزانة التشفير العسكري AES-256-GCM وتمويه الملفات).
   - *مثال:* من `2.5.0` إلى `2.6.0`.

3. **التغييرات الجوهرية الكبرى (MAJOR - `X.0.0`):**
   - تغييرات معمارية كبرى غير متوافقة عكسياً (Breaking Changes).
   - إعادة كتابة كاملة للمحرك أو واجهة المستخدم أو نموذج التخزين.
   - *مثال:* من `2.6.0` إلى `3.0.0`.

---

## 2. مصفوفة مزامنة رقم الإصدار (Version Sync Matrix)
عند كل ترقية للإصدار، **يجب تحديث رقم الإصدار في كافة الملفات التالية معاً وبشكل متطابق تماماً**:
1. **`Cargo.toml`**: حقل `version = "X.Y.Z"`.
2. **`installer.iss`**: السطر `#define MyAppVersion "X.Y.Z"`.
3. **`src/ui/index.html`**:
   - الثابت البرمجي: `const APP_CURRENT_VERSION = 'X.Y.Z';`
   - شارة نافذة حول: `<div ...>Version X.Y.Z Pro (Windows x64)</div>`
   - شارة نافذة التحديث: `<div id="updateCurrentBadge" ...>Current: vX.Y.Z</div>`
4. **اسم حزمة التثبيت الناتجة**: `dist\90FTP-Setup-vX.Y.Z.exe`.

---

## 3. خط أنابيب البناء والتوقيع الإلزامي (Build & Verification Pipeline)
قبل تنفيذ أي `git commit` أو `git push` لإصدار جديد، يجب تنفيذ الخطوات التالية بالترتيب:
1. **تشغيل الاختبارات التلقائية:**
   ```powershell
   cargo test --bin ninety_ftp
   ```
2. **بناء النسخة النهائية المحسنة:**
   ```powershell
   cargo build --release
   ```
3. **تحديث الملف التنفيذي الرئيسي:**
   ```powershell
   Copy-Item -Path "target\release\ninety_ftp.exe" -Destination "90FTP.exe" -Force
   ```
4. **توقيع الملف التنفيذي برمز الناشر aljailane والختم الزمني المشفر DigiCert RFC 3161:**
   ```powershell
   & "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe" sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Client" "90FTP.exe"
   ```
5. **بناء وتجميع حزمة التثبيت الأنيقة:**
   ```powershell
   & "C:\Program Files\Inno Setup 7\ISCC.exe" installer.iss
   ```
6. **توقيع حزمة التثبيت الناتجة:**
   ```powershell
   & "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe" sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Setup" "dist\90FTP-Setup-vX.Y.Z.exe"
   ```
7. **الالتزام في Git مع رسالة توضيحية ومطابقة لنمط Conventional Commits:**
   ```powershell
   git add .
   git commit -m "release: bump version to vX.Y.Z with [feature/fix summary]"
   git push origin main
   ```
