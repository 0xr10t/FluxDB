// Will be serialized before storing on disk
pub struct SlottedPage {
    keys: Vec<PageKeys>,
    data: Vec<>
}

// Keys go from top-down, data is appended bottom-up
pub struct PageKeys {}

impl SlottedPage {
    pub fn new() -> Self {}

    // Use internal functions for modifying page
    pub fn add_data(keys: PageKeys, data: &[u8]) -> Result<> {}

    pub fn modify_data(keys: PageKeys, new_data: &[u8]) -> Result<> {}

    pub fn delete_data(keys: PageKeys, new_data: &[u8]) -> Result<> {}
}