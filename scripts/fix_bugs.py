import os
base = r"D:\Desktop\Wen\06 Codex\clipboard"

# Fix main.js
p = os.path.join(base, "main.js")
with open(p, "r", encoding="utf-8") as f:
    t = f.read()

old = (
    '  // 启用 Windows 毛玻璃效果\n'
    '  if (process.platform === "win32") {\n'
    '    try {\n'
    '      mainWindow.setBackgroundMaterial("acrylic");\n'
    '      console.log("[window] acrylic enabled");\n'
    '    } catch (e) {\n'
    '      try {\n'
    '        mainWindow.setBackgroundMaterial("mica");\n'
    '        console.log("[window] mica enabled");\n'
    '      } catch (e2) {\n'
    '        console.log("[window] acrylic/mica not available, use css blur");\n'
    '      }\n'
    '    }\n'
    '  }'
)

new = (
    '  // 启用 Windows 毛玻璃效果（增强版）\n'
    '  if (process.platform === "win32") {\n'
    '    let glassOk = false;\n'
    '    try {\n'
    '      mainWindow.setBackgroundMaterial("acrylic");\n'
    '      console.log("[window] acrylic enabled");\n'
    '      glassOk = true;\n'
    '    } catch (e) {\n'
    '      console.log("[window] acrylic failed:", e.message);\n'
    '    }\n'
    '    if (!glassOk) {\n'
    '      try {\n'
    '        mainWindow.setBackgroundMaterial("mica");\n'
    '        console.log("[window] mica enabled");\n'
    '        glassOk = true;\n'
    '      } catch (e2) {\n'
    '        console.log("[window] mica failed:", e2.message);\n'
    '      }\n'
    '    }\n'
    '    if (!glassOk) {\n'
    '      console.log("[window] glass unavailable, fallback to css");\n'
    '      mainWindow.setBackgroundMaterial("none");\n'
    '    }\n'
    '  }'
)

if old in t:
    t = t.replace(old, new)
    with open(p, "w", encoding="utf-8") as f:
        f.write(t)
    print("main.js: OK")
else:
    print("main.js: OLD NOT FOUND - checking approximations...")
    # Count occurrences
    print("'acrylic' count:", t.count("acrylic"))

# Fix app.js
p2 = os.path.join(base, "renderer", "app.js")
with open(p2, "r", encoding="utf-8") as f:
    t2 = f.read()

old2 = (
    '  // 全窗拖拽\n'
    '  let dragA = false, dragSX, dragSY;\n'
    '  document.querySelector(".window").addEventListener("mousedown", (e) => {\n'
    '    if (e.target.closest(".win-btn") || e.target.closest(".search-box") || \n'
    '        e.target.closest(".item") || e.target.closest(".btn-clear") ||\n'
    '        e.target.closest(".settings-panel") || e.target.closest(".context-menu")) return;\n'
    '    dragA = true; dragSX = e.screenX; dragSY = e.screenY;\n'
    '  });\n'
    '  document.addEventListener("mousemove", (e) => {\n'
    '    if (!dragA) return;\n'
    '    window.clipboardAPI.dragWindow(e.screenX - dragSX, e.screenY - dragSY);\n'
    '    dragSX = e.screenX; dragSY = e.screenY;\n'
    '  });\n'
    '  document.addEventListener("mouseup", () => { dragA = false; });'
)

new2 = (
    '  // 全窗拖拽（排除窗口边缘6px缩放区 + 交互元素）\n'
    '  let dragA = false, dragSX, dragSY;\n'
    '  document.querySelector(".window").addEventListener("mousedown", (e) => {\n'
    '    if (e.target.closest(".win-btn") || e.target.closest(".search-box") || \n'
    '        e.target.closest(".item") || e.target.closest(".btn-clear") ||\n'
    '        e.target.closest(".settings-panel") || e.target.closest(".context-menu")) return;\n'
    '    // 排除窗口边缘6px（系统隐形缩放区），避免拖拽与系统缩放冲突\n'
    '    const r = e.currentTarget.getBoundingClientRect();\n'
    '    const edge = 6;\n'
    '    if (e.clientX - r.left < edge || r.right - e.clientX < edge ||\n'
    '        e.clientY - r.top < edge || r.bottom - e.clientY < edge) return;\n'
    '    dragA = true; dragSX = e.screenX; dragSY = e.screenY;\n'
    '  });\n'
    '  document.addEventListener("mousemove", (e) => {\n'
    '    if (!dragA) return;\n'
    '    const dx = e.screenX - dragSX, dy = e.screenY - dragSY;\n'
    '    if (dx !== 0 || dy !== 0) {\n'
    '      window.clipboardAPI.dragWindow(dx, dy);\n'
    '      dragSX = e.screenX; dragSY = e.screenY;\n'
    '    }\n'
    '  });\n'
    '  document.addEventListener("mouseup", () => { dragA = false; });'
)

if old2 in t2:
    t2 = t2.replace(old2, new2)
    with open(p2, "w", encoding="utf-8") as f:
        f.write(t2)
    print("app.js: OK")
else:
    print("app.js: OLD NOT FOUND")
    i = t2.find("全窗拖拽")
    if i >= 0:
        print("Found at index", i)
        print(repr(t2[i:i+400]))
