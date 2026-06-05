const fs = require("fs");
const path = require("path");

module.exports = async function afterPack(context) {
    const { appOutDir } = context;
    let cleaned = 0;

    // ?????????? zh-CN + en-US?
    const localeDir = path.join(appOutDir, "locales");
    if (fs.existsSync(localeDir)) {
        for (const f of fs.readdirSync(localeDir)) {
            if (f.endsWith(".pak") && f !== "zh-CN.pak" && f !== "en-US.pak") {
                cleaned += fs.statSync(path.join(localeDir, f)).size;
                fs.rmSync(path.join(localeDir, f));
            }
        }
    }

    // ?? elevate.exe??????????
    const el = path.join(appOutDir, "resources", "elevate.exe");
    if (fs.existsSync(el)) {
        cleaned += fs.statSync(el).size;
        fs.rmSync(el);
    }

    // ????? DLL???????/GPU/?????
    const safeDlls = [
        "ffmpeg.dll",          // ???????????
        "d3dcompiler_47.dll",  // ???? DirectX ????Electron ????????
    ];
    for (const dll of safeDlls) {
        const p = path.join(appOutDir, dll);
        if (fs.existsSync(p)) {
            cleaned += fs.statSync(p).size;
            fs.rmSync(p);
        }
    }

    console.log("[afterPack] cleaned " + (cleaned / 1024 / 1024).toFixed(1) + " MB");
};
