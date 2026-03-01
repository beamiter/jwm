impl :: bincode :: Encode for MonitorInfo
{
    fn encode < __E : :: bincode :: enc :: Encoder >
    (& self, encoder : & mut __E) ->core :: result :: Result < (), :: bincode
    :: error :: EncodeError >
    {
        :: bincode :: Encode :: encode(&self.monitor_num, encoder) ?; ::
        bincode :: Encode :: encode(&self.monitor_width, encoder) ?; ::
        bincode :: Encode :: encode(&self.monitor_height, encoder) ?; ::
        bincode :: Encode :: encode(&self.monitor_x, encoder) ?; :: bincode ::
        Encode :: encode(&self.monitor_y, encoder) ?; :: bincode :: Encode ::
        encode(&self.tag_status_vec, encoder) ?; :: bincode :: Encode ::
        encode(&self.client_name, encoder) ?; :: bincode :: Encode ::
        encode(&self.ltsymbol, encoder) ?; core :: result :: Result :: Ok(())
    }
}