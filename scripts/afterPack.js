const fs = require("fs");
const path = require("path");

module.exports = async function afterPack(context) {
    const { appOutDir } = context;
    let deleted = 0;

    // 删除多余语言包（保留 zh-CN + en-US）
    const localeDir = path.join(appOutDir, "locales");
    if (fs.existsSync(localeDir)) {
        for (const f of fs.readdirSync(localeDir)) {
            if (f.endsWith(".pak") && f !== "zh-CN.pak" && f !== "en-US.pak") {
                const p = path.join(localeDir, f);
                deleted += fs.statSync(p).size;
                fs.rmSync(p);
            }
        }
    }

    // 删除无关 DLL
    const safeRemove = [
        "ffmpeg.dll",          // 视频解码，本程序不需要
        "d3dcompiler_47.dll",  // 系统自带 DirectX 编译器
    ];
    for (const f of safeRemove) {
        const p = path.join(appOutDir, f);
        if (fs.existsSync(p)) {
            deleted += fs.statSync(p).size;
            fs.rmSync(p);
        }
    }

    // 删除 elevate.exe（不需要管理员提权）
    const el = path.join(appOutDir, "resources", "elevate.exe");
    if (fs.existsSync(el)) {
        deleted += fs.statSync(el).size;
        fs.rmSync(el);
    }

    console.log("[afterPack] cleaned " + (deleted / 1024 / 1024).toFixed(1) + " MB");
};
