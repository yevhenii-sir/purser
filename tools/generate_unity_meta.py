"""Creates missing .meta files for the Unity package (UPM packages must ship them).

GUIDs are derived from the package-relative path, so re-running never changes existing assets.
Folders ending with '~' (Samples~, Documentation~) are skipped: Unity does not import them.

    python tools/generate_unity_meta.py
"""
import hashlib
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent / "unity" / "com.yevhenii.purser"
PACKAGE = "com.yevhenii.purser"

TEMPLATES = {
    "folder": "DefaultImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n",
    ".cs": ("MonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n  defaultReferences: []\n"
            "  executionOrder: 0\n  icon: {instanceID: 0}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"),
    ".asmdef": "AssemblyDefinitionImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n",
    "package.json": "PackageManifestImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n",
    # Native iOS source: compiled into the Xcode project on iOS only.
    ".mm": ("PluginImporter:\n  externalObjects: {}\n  serializedVersion: 2\n  iconMap: {}\n  executionOrder: {}\n"
            "  defineConstraints: []\n  isPreloaded: 0\n  isOverridable: 0\n  isExplicitlyReferenced: 0\n"
            "  validateReferences: 1\n  platformData:\n  - first:\n      Any: \n    second:\n      enabled: 0\n"
            "      settings: {}\n  - first:\n      Editor: Editor\n    second:\n      enabled: 0\n      settings:\n"
            "        DefaultValueInitialized: true\n  - first:\n      iPhone: iOS\n    second:\n      enabled: 1\n"
            "      settings: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"),
    "text": "TextScriptImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n",
}


def guid(rel: str) -> str:
    return hashlib.md5(f"{PACKAGE}/{rel}".encode("utf-8")).hexdigest()


def meta_for(path: pathlib.Path) -> str:
    rel = path.relative_to(ROOT).as_posix()
    if path.is_dir():
        body = "folderAsset: yes\n" + TEMPLATES["folder"]
    elif path.name == "package.json":
        body = TEMPLATES["package.json"]
    elif path.suffix in (".cs", ".asmdef", ".mm"):
        body = TEMPLATES[path.suffix]
    else:
        body = TEMPLATES["text"]
    return f"fileFormatVersion: 2\nguid: {guid(rel)}\n{body}"


def main():
    created = 0
    for path in sorted(ROOT.rglob("*")):
        rel_parts = path.relative_to(ROOT).parts
        if any(p.endswith("~") or p.startswith(".") for p in rel_parts) or path.suffix == ".meta":
            continue
        meta = path.with_name(path.name + ".meta")
        if not meta.exists():
            meta.write_text(meta_for(path), encoding="utf-8", newline="\n")
            created += 1
    # Drop metas whose asset was deleted.
    removed = 0
    for meta in ROOT.rglob("*.meta"):
        if not meta.with_name(meta.name[:-5]).exists():
            meta.unlink()
            removed += 1
    print(f"created {created}, removed {removed} .meta files")


if __name__ == "__main__":
    main()
