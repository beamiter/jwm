from pathlib import Path

path = Path("src/jwm/features/toggles.rs")
data = path.read_text(encoding="utf-8")
old = '''        if !backend.input_ops().grab_pointer(pointer_mask, crosshair)? {
            let _ = backend.key_ops().ungrab_keyboard();
            return Err("could not grab pointer for recording region selection".into());
        }
'''
new = '''        match backend.input_ops().grab_pointer(pointer_mask, crosshair) {
            Ok(true) => {}
            Ok(false) => {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err("could not grab pointer for recording region selection".into());
            }
            Err(error) => {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err(error.into());
            }
        }
'''
count = data.count(old)
if count != 1:
    raise RuntimeError(f"expected one recording grab match, found {count}")
path.write_text(data.replace(old, new, 1), encoding="utf-8")
