use eframe::egui::{
    ColorImage, Context, IconData, Id, Image, TextureHandle, TextureOptions, Ui, vec2,
};

const PNG: &[u8] = include_bytes!("../assets/app-icon.png");
const BRAND: &str = "godovoy-plan-brand";

pub fn window_icon() -> IconData {
    eframe::icon_data::from_png_bytes(PNG).expect("bundled application icon must be a valid PNG")
}

pub fn install(ctx: &Context) {
    let icon = window_icon();
    let image =
        ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
    let texture = ctx.load_texture(BRAND, image, TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(Id::new(BRAND), texture));
}

pub fn show(ui: &mut Ui) {
    if let Some(texture) = ui
        .ctx()
        .data(|data| data.get_temp::<TextureHandle>(Id::new(BRAND)))
    {
        ui.add(Image::new((texture.id(), vec2(28.0, 28.0))));
    }
}
