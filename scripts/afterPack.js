const fs = require('fs');
const path = require('path');

module.exports = async function afterPack(context) {
    const { appOutDir } = context;

    // 删除多余语言包（仅保留 zh-CN + en-US）
    const localeDir = path.join(appOutDir, 'locales');
    if (fs.existsSync(localeDir)) {
        for (const f of fs.readdirSync(localeDir)) {
            if (f.endsWith('.pak') && f !== 'zh-CN.pak' && f !== 'en-US.pak') {
                fs.rmSync(path.join(localeDir, f));
            }
        }
    }

    // 删除无用 DLL 和文件
    const remove = [
        'dxcompiler.dll',
        'vk_swiftshader.dll',
        'libGLESv2.dll',
        'libEGL.dll',
        'dxil.dll',
        'ffmpeg.dll',
        'd3dcompiler_47.dll',
        'resources/elevate.exe'
    ];
    for (const f of remove) {
        const p = path.join(appOutDir, f);
        if (fs.existsSync(p)) fs.rmSync(p);
    }
};
