const fs = require("fs");
const path = require("path");

module.exports = async function afterPack(context) {
    const { appOutDir } = context;
    let cleaned = 0;

    // ??????????? zh-CN + en-US?????? DLL/exe
    const localeDir = path.join(appOutDir, "locales");
    if (fs.existsSync(localeDir)) {
        for (const f of fs.readdirSync(localeDir)) {
            if (f.endsWith(".pak") && f !== "zh-CN.pak" && f !== "en-US.pak") {
                cleaned += fs.statSync(path.join(localeDir, f)).size;
                fs.rmSync(path.join(localeDir, f));
            }
        }
    }

    console.log("[afterPack] cleaned locales: " + (cleaned / 1024 / 1024).toFixed(1) + " MB");
};
