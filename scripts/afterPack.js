// afterPack hook — 构建完成后清理无用文件
exports.default = async function(context) {
  const { appOutDir, electronVersion, platform, arch } = context;
  if (platform.name !== 'win32') return;

  const path = require('path');
  const fs = require('fs');

  let deletedBytes = 0;

  // 1. 保留 zh-CN + en-US 语言包
  const localeDir = path.join(appOutDir, 'locales');
  if (fs.existsSync(localeDir)) {
    const keep = new Set(['zh-CN.pak', 'en-US.pak']);
    for (const f of fs.readdirSync(localeDir)) {
      if (f.endsWith('.pak') && !keep.has(f)) {
        const stats = fs.statSync(path.join(localeDir, f));
        deletedBytes += stats.size;
        fs.unlinkSync(path.join(localeDir, f));
      }
    }
  }

  // 2. 删除无用 DLL
  const unusedDlls = [
    'dxcompiler.dll', 'vk_swiftshader.dll', 'libGLESv2.dll',
    'libEGL.dll', 'dxil.dll', 'ffmpeg.dll', 'd3dcompiler_47.dll'
  ];
  for (const dll of unusedDlls) {
    const p = path.join(appOutDir, dll);
    if (fs.existsSync(p)) {
      deletedBytes += fs.statSync(p).size;
      fs.unlinkSync(p);
    }
  }

  // 3. 删除 elevate.exe
  const elevatePath = path.join(appOutDir, 'resources', 'elevate.exe');
  if (fs.existsSync(elevatePath)) {
    deletedBytes += fs.statSync(elevatePath).size;
    fs.unlinkSync(elevatePath);
  }

  const savedMB = (deletedBytes / 1024 / 1024).toFixed(1);
  console.log(`[cleanup] 已释放 ${savedMB} MB`);
};
