impl :: bincode :: Encode for SharedMessage
{
    fn encode < __E : :: bincode :: enc :: Encoder >
    (& self, encoder : & mut __E) ->core :: result :: Result < (), :: bincode
    :: error :: EncodeError >
    {
        :: bincode :: Encode :: encode(&self.timestamp, encoder) ?; :: bincode
        :: Encode :: encode(&self.monitor_info, encoder) ?; core :: result ::
        Result :: Ok(())
    }
}