use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pdfium_render::prelude::Pdfium;

pub(crate) struct TestPdfium {
    pdfium: Arc<Pdfium>,
    _access: MutexGuard<'static, ()>,
}

impl TestPdfium {
    pub(crate) fn shared(&self) -> Arc<Pdfium> {
        self.pdfium.clone()
    }
}

impl std::ops::Deref for TestPdfium {
    type Target = Pdfium;

    fn deref(&self) -> &Self::Target {
        &self.pdfium
    }
}

pub(crate) fn test_pdfium() -> Option<TestPdfium> {
    static PDFIUM: OnceLock<Option<Arc<Pdfium>>> = OnceLock::new();
    static ACCESS: Mutex<()> = Mutex::new(());

    let access = ACCESS.lock().unwrap_or_else(|error| error.into_inner());
    let pdfium = PDFIUM
        .get_or_init(|| {
            let bindings = std::env::var("PDF_TOOLS_PDFIUM_PATH")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .and_then(|path| Pdfium::bind_to_library(path.trim()).ok())
                .or_else(|| Pdfium::bind_to_system_library().ok());
            bindings.map(Pdfium::new).map(Arc::new)
        })
        .clone()?;
    Some(TestPdfium {
        pdfium,
        _access: access,
    })
}
