const fs = require("fs");
const path = require("path");

module.exports = async function afterPack(context) {
    const { appOutDir } = context;
    let deleted = 0;

    // ??????????? zh-CN + en-US?????? DLL
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

    console.log("[afterPack] cleaned " + (deleted / 1024 / 1024).toFixed(1) + " MB (language packs only)");
};
