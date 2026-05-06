class UserRepository:
    def __init__(self, db_session):
        self.db = db_session

    def find_by_id(self, user_id: int):
        return self.db.query(User).get(user_id)

    def find_all(self):
        return self.db.query(User).all()


class NotificationService:
    def __init__(self, email_client, sms_client):
        self.email = email_client
        self.sms = sms_client

    def send_welcome(self, user):
        self.email.send(user.email, "Welcome!")


def calculate_discount(price: float, percentage: float) -> float:
    return price * (1 - percentage / 100)


@property
def is_active(self):
    return self.status == "active"
