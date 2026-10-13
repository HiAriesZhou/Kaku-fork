pub(crate) fn prefer_swrast() -> bool {
    config::configuration().front_end == config::FrontEndSelection::Software
}
