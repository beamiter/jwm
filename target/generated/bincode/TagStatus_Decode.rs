impl < __Context > :: bincode :: Decode < __Context > for TagStatus
{
    fn decode < __D : :: bincode :: de :: Decoder < Context = __Context > >
    (decoder : & mut __D) ->core :: result :: Result < Self, :: bincode ::
    error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            is_selected : :: bincode :: Decode :: decode(decoder) ?, is_urg :
            :: bincode :: Decode :: decode(decoder) ?, is_filled : :: bincode
            :: Decode :: decode(decoder) ?, is_occ : :: bincode :: Decode ::
            decode(decoder) ?,
        })
    }
} impl < '__de, __Context > :: bincode :: BorrowDecode < '__de, __Context >
for TagStatus
{
    fn borrow_decode < __D : :: bincode :: de :: BorrowDecoder < '__de,
    Context = __Context > > (decoder : & mut __D) ->core :: result :: Result <
    Self, :: bincode :: error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            is_selected : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, is_urg : :: bincode :: BorrowDecode ::<
            '_, __Context >:: borrow_decode(decoder) ?, is_filled : :: bincode
            :: BorrowDecode ::< '_, __Context >:: borrow_decode(decoder) ?,
            is_occ : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?,
        })
    }
}