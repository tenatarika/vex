package billing

type InvoiceService struct {
	repo InvoiceRepository
}

type InvoiceRepository interface {
	FindByID(id string) (*Invoice, error)
	Save(invoice *Invoice) error
}

func NewInvoiceService(repo InvoiceRepository) *InvoiceService {
	return &InvoiceService{repo: repo}
}

func (s *InvoiceService) CreateInvoice(customerID string, amount float64) (*Invoice, error) {
	invoice := &Invoice{
		CustomerID: customerID,
		Amount:     amount,
		Status:     StatusDraft,
	}
	return invoice, s.repo.Save(invoice)
}
