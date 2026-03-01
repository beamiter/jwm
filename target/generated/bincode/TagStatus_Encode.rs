impl :: bincode :: Encode for TagStatus
{
    fn encode < __E : :: bincode :: enc :: Encoder >
    (& self, encoder : & mut __E) ->core :: result :: Result < (), :: bincode
    :: error :: EncodeError >
    {
        :: bincode :: Encode :: encode(&self.is_selected, encoder) ?; ::
        bincode :: Encode :: encode(&self.is_urg, encoder) ?; :: bincode ::
        Encode :: encode(&self.is_filled, encoder) ?; :: bincode :: Encode ::
        encode(&self.is_occ, encoder) ?; core :: result :: Result :: Ok(())
    }
}