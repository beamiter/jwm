impl < __Context > :: bincode :: Decode < __Context > for SharedMessage
{
    fn decode < __D : :: bincode :: de :: Decoder < Context = __Context > >
    (decoder : & mut __D) ->core :: result :: Result < Self, :: bincode ::
    error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            timestamp : :: bincode :: Decode :: decode(decoder) ?,
            monitor_info : :: bincode :: Decode :: decode(decoder) ?,
        })
    }
} impl < '__de, __Context > :: bincode :: BorrowDecode < '__de, __Context >
for SharedMessage
{
    fn borrow_decode < __D : :: bincode :: de :: BorrowDecoder < '__de,
    Context = __Context > > (decoder : & mut __D) ->core :: result :: Result <
    Self, :: bincode :: error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            timestamp : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, monitor_info : :: bincode ::
            BorrowDecode ::< '_, __Context >:: borrow_decode(decoder) ?,
        })
    }
}