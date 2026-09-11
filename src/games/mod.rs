use crate::state::GameMetrics;

pub trait GameProfile: Send + Sync {
    fn id(&self) -> &'static str;
    
    fn parse_telemetry(&self, img: &DynamicImage) -> GameMetrics;
       
    fn get_system_instructions(&self) -> &'static str;
}